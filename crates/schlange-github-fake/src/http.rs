//! The fake's REST endpoints. Only what Schlange's client uses, with GitHub's status codes
//! for the cases the client distinguishes (404, 405, 409, 422, 304).

use serde_json::{Value, json};

use schlange_github::time::unix_now;

use crate::{FakePr, FakeToken, FaultKind, Inner, Squash, St, ts};

pub(crate) struct Reply {
    pub status: u16,
    pub body: Value,
    pub headers: Vec<(String, String)>,
}

fn reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        body,
        headers: vec![],
    }
}

fn err(status: u16, message: &str) -> Reply {
    reply(status, json!({ "message": message }))
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pages a list: `per_page`, `page`, and a `Link` header with `rel="next"`.
fn paginate(
    base_url: &str,
    path: &str,
    query: &str,
    items: Vec<Value>,
    wrap: Option<&str>,
) -> Reply {
    let per_page: usize = query_param(query, "per_page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let page: usize = query_param(query, "page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let total = items.len();
    let slice: Vec<Value> = items
        .into_iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .collect();
    let body = match wrap {
        Some(k) => json!({ "total_count": total, k: slice }),
        None => Value::Array(slice),
    };
    let mut r = reply(200, body);
    if page * per_page < total {
        let rest: Vec<&str> = query
            .split('&')
            .filter(|kv| !kv.starts_with("page=") && !kv.is_empty())
            .collect();
        let mut q = rest.join("&");
        if !q.is_empty() {
            q.push('&');
        }
        r.headers.push((
            "link".into(),
            format!("<{base_url}{path}?{q}page={}>; rel=\"next\"", page + 1),
        ));
    }
    r
}

enum Caller {
    App,
    Installation,
}

fn authenticate(st: &mut St, inner: &Inner, auth: Option<&str>) -> Option<Caller> {
    st.caller_token = None;
    let a = auth?;
    let tok = a
        .strip_prefix("Bearer ")
        .or_else(|| a.strip_prefix("token "))?;
    if tok.matches('.').count() == 2 {
        let claims = schlange_github::jwt::verify(tok, &inner.public_key)?;
        let iss_ok = claims["iss"].as_str() == Some(&st.cfg.app_id.to_string())
            || claims["iss"].as_u64() == Some(st.cfg.app_id);
        let exp_ok = claims["exp"].as_u64().is_some_and(|e| e > unix_now());
        return (iss_ok && exp_ok).then_some(Caller::App);
    }
    let ok = st.tokens.get(tok).is_some_and(|t| t.expires > unix_now());
    if ok {
        st.caller_token = Some(tok.to_string());
    }
    ok.then_some(Caller::Installation)
}

fn rank(level: &str) -> u8 {
    match level {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

pub(crate) fn serve(server: &tiny_http::Server, inner: &Inner) {
    loop {
        if inner.stop.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let mut req = match server.recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        if inner.stop.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let method = req.method().to_string();
        let url = req.url().to_string();
        let header = |name: &str| {
            req.headers()
                .iter()
                .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                .map(|h| h.value.as_str().to_string())
        };
        let auth = header("authorization");
        let inm = header("if-none-match");
        let mut body = Vec::new();
        let _ = req.as_reader().read_to_end(&mut body);
        let base_url = format!("http://{}", server.server_addr());
        let r = {
            let mut st = inner.st.lock().expect("fake state");
            let r = handle(
                &mut st,
                inner,
                &base_url,
                &method,
                &url,
                auth.as_deref(),
                &body,
            );
            let path = url.split('?').next().unwrap_or("").to_string();
            st.requests.push(format!("{method} {path} {}", r.status));
            r
        };
        inner.outbox_cv.notify_all();
        let text = if r.status == 204 {
            String::new()
        } else {
            r.body.to_string()
        };
        let etag = format!(
            "\"{}\"",
            schlange_github::webhook::hex(
                &ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref()[..8]
            )
        );
        let (status, text) = if method == "GET" && r.status == 200 && inm.as_deref() == Some(&etag)
        {
            (304, String::new())
        } else {
            (r.status, text)
        };
        let mut resp = tiny_http::Response::from_string(text).with_status_code(status);
        let mut headers = r.headers;
        headers.push(("content-type".into(), "application/json".into()));
        headers.push(("x-ratelimit-limit".into(), "5000".into()));
        if !headers.iter().any(|(k, _)| k == "x-ratelimit-remaining") {
            headers.push(("x-ratelimit-remaining".into(), "4999".into()));
        }
        if method == "GET" && r.status == 200 {
            headers.push(("etag".into(), etag));
        }
        for (k, v) in headers {
            if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                resp = resp.with_header(h);
            }
        }
        let _ = req.respond(resp);
    }
}

fn handle(
    st: &mut St,
    inner: &Inner,
    base_url: &str,
    method: &str,
    url: &str,
    auth: Option<&str>,
    body: &[u8],
) -> Reply {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let path = decode(path);
    // A renamed repository: GitHub redirects the old name (301 for reads, 307 otherwise).
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if seg.len() >= 3
        && seg[0] == "repos"
        && seg[1] == st.cfg.owner
        && st.old_names.iter().any(|o| o == seg[2])
    {
        let mut rest = seg.clone();
        rest[2] = &st.cfg.name;
        let q = if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        };
        let mut r = err(if method == "GET" { 301 } else { 307 }, "Moved Permanently");
        r.headers.push((
            "location".into(),
            format!("{base_url}/{}{q}", rest.join("/")),
        ));
        return r;
    }
    // Injected faults first.
    let mut lie = None;
    if let Some(i) = st.faults.iter().position(|f| {
        (f.method == "*" || f.method == method) && path.contains(&f.path) && f.times > 0
    }) {
        st.faults[i].times -= 1;
        let kind = st.faults[i].kind.clone();
        let then = if st.faults[i].fired {
            vec![]
        } else {
            st.faults[i].fired = true;
            st.faults[i].then.clone()
        };
        if st.faults[i].times == 0 {
            st.faults.remove(i);
        }
        st.run_then(&then);
        match kind {
            FaultKind::Status(s) => return err(s, "injected fault"),
            FaultKind::StatusMessage(s, m) => return err(s, &m),
            FaultKind::RateLimit { retry_after } => {
                let mut r = err(429, "API rate limit exceeded");
                r.headers
                    .push(("retry-after".into(), retry_after.to_string()));
                return r;
            }
            FaultKind::SecondaryLimit { retry_after } => {
                let mut r = err(403, "You have exceeded a secondary rate limit.");
                r.headers
                    .push(("retry-after".into(), retry_after.to_string()));
                return r;
            }
            FaultKind::Lie(s) => lie = Some(s),
        }
    }
    let caller = authenticate(st, inner, auth);
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let r = route(st, caller, base_url, method, &path, query, &body);
    match lie {
        Some(s) => err(s, "injected lie: the request was carried out"),
        None => r,
    }
}

fn route(
    st: &mut St,
    caller: Option<Caller>,
    base_url: &str,
    method: &str,
    path: &str,
    query: &str,
    body: &Value,
) -> Reply {
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let Some(caller) = caller else {
        return err(401, "Bad credentials");
    };
    // App endpoints (JWT).
    if seg.first() == Some(&"app") {
        if !matches!(caller, Caller::App) {
            return err(401, "A JSON web token could not be decoded");
        }
        return match (method, &seg[1..]) {
            ("GET", []) => reply(
                200,
                json!({ "id": st.cfg.app_id, "slug": st.cfg.app_slug, "name": "Schlange (test)" }),
            ),
            ("GET", ["installations", id]) if id.parse() == Ok(st.cfg.installation_id) => reply(
                200,
                json!({ "id": st.cfg.installation_id, "permissions": st.cfg.permissions, "events": schlange_github::permissions::EVENTS }),
            ),
            ("POST", ["installations", id, "access_tokens"])
                if id.parse() == Ok(st.cfg.installation_id) =>
            {
                st.token_requests.push(body.clone());
                let repositories = body["repositories"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                });
                if let Some(r) = &repositories
                    && r.iter().any(|n| *n != st.cfg.name)
                {
                    return err(
                        422,
                        "There is at least one repository that does not exist or is not accessible to the parent installation.",
                    );
                }
                let mut permissions = st.cfg.permissions.clone();
                if let Some(req) = body["permissions"].as_object() {
                    permissions.clear();
                    for (k, v) in req {
                        let want = v.as_str().unwrap_or("");
                        let have = st.cfg.permissions.get(k).map(String::as_str).unwrap_or("");
                        if rank(want) > rank(have) {
                            return err(
                                422,
                                "The permissions requested are not granted to this installation.",
                            );
                        }
                        permissions.insert(k.clone(), want.to_string());
                    }
                }
                let n = st.id();
                let token = format!("ghs_fake_{n}");
                let exp = unix_now() + 3600;
                st.tokens.insert(
                    token.clone(),
                    FakeToken {
                        expires: exp,
                        repositories,
                        permissions,
                    },
                );
                reply(201, json!({ "token": token, "expires_at": ts(exp) }))
            }
            ("GET", ["hook", "deliveries"]) => {
                let items: Vec<Value> = st
                    .deliveries
                    .iter()
                    .rev()
                    .map(|d| {
                        json!({ "id": d.id, "guid": d.guid, "event": d.event,
                                "delivered_at": ts(d.delivered_at), "status_code": d.status_code })
                    })
                    .collect();
                paginate(base_url, path, query, items, None)
            }
            _ => err(404, "Not Found"),
        };
    }
    if seg == ["rate_limit"] {
        return reply(
            200,
            json!({ "resources": { "core": { "limit": 5000, "remaining": 4999, "reset": unix_now() + 3600 } } }),
        );
    }
    if seg.len() < 3 || seg[0] != "repos" || seg[1] != st.cfg.owner || seg[2] != st.cfg.name {
        return err(404, "Not Found");
    }
    if seg.get(3) == Some(&"installation") {
        return if matches!(caller, Caller::App) {
            reply(
                200,
                json!({ "id": st.cfg.installation_id, "permissions": st.cfg.permissions }),
            )
        } else {
            err(401, "needs a JWT")
        };
    }
    if !matches!(caller, Caller::Installation) {
        return err(401, "needs an installation token");
    }
    let limited = st
        .caller_token
        .as_ref()
        .and_then(|t| st.tokens.get(t))
        .and_then(|t| t.repositories.clone());
    if limited.is_some_and(|r| !r.contains(&st.cfg.name)) {
        return err(404, "Not Found");
    }
    let rest = &seg[3..];
    repo_route(st, base_url, method, path, query, rest, body)
}

fn new_pr_body(st: &St) -> String {
    format!("{}[bot]", st.cfg.app_slug)
}

#[allow(clippy::too_many_lines)]
fn repo_route(
    st: &mut St,
    base_url: &str,
    method: &str,
    path: &str,
    query: &str,
    rest: &[&str],
    body: &Value,
) -> Reply {
    let num = |s: &str| s.parse::<u64>().ok();
    match (method, rest) {
        ("GET", []) => reply(200, st.repo_json()),
        ("GET", ["pulls"]) => {
            let state = query_param(query, "state").unwrap_or("open");
            let head = query_param(query, "head").map(decode);
            let numbers: Vec<u64> = st
                .prs
                .values()
                .filter(|p| match state {
                    "open" => p.open,
                    "closed" => !p.open,
                    _ => true,
                })
                .filter(|p| {
                    head.as_ref()
                        .is_none_or(|h| *h == format!("{}:{}", st.cfg.owner, p.head_ref))
                })
                .map(|p| p.number)
                .collect();
            let items = numbers.into_iter().filter_map(|n| st.pr_json(n)).collect();
            paginate(base_url, path, query, items, None)
        }
        ("POST", ["pulls"]) => {
            let head = body["head"].as_str().unwrap_or("").to_string();
            let base = body["base"].as_str().unwrap_or("").to_string();
            let Some(head_sha) = st.tip(&head) else {
                return err(422, "Validation Failed: head does not exist");
            };
            if st.tip(&base).is_none() {
                return err(422, "Validation Failed: base does not exist");
            }
            if st.prs.values().any(|p| p.open && p.head_ref == head) {
                return err(422, "A pull request already exists");
            }
            st.next_issue += 1;
            let n = st.next_issue;
            let user = new_pr_body(st);
            st.prs.insert(
                n,
                FakePr {
                    number: n,
                    title: body["title"].as_str().unwrap_or("").into(),
                    body: body["body"].as_str().unwrap_or("").into(),
                    user,
                    head_ref: head,
                    head_sha,
                    base,
                    draft: body["draft"].as_bool().unwrap_or(false),
                    open: true,
                    merged: false,
                    merge_commit_sha: None,
                    labels: Default::default(),
                },
            );
            st.emit_pr("opened", n, json!({}));
            st.run_ci(n);
            reply(201, st.pr_json(n).unwrap_or_default())
        }
        ("GET", ["pulls", n]) => {
            let Some(n) = num(n) else {
                return err(404, "Not Found");
            };
            if let Some(stale) = st.stale_read(&format!("pull:{n}")) {
                return reply(200, stale);
            }
            match st.pr_json(n) {
                Some(mut j) => {
                    if let Some(left) = st.mergeable_unknown.get_mut(&n) {
                        *left = left.saturating_sub(1);
                        if *left == 0 {
                            st.mergeable_unknown.remove(&n);
                        }
                        j["mergeable"] = Value::Null;
                    }
                    reply(200, j)
                }
                None => err(404, "Not Found"),
            }
        }
        ("PATCH", ["pulls", n]) => {
            let Some(n) = num(n).filter(|n| st.prs.contains_key(n)) else {
                return err(404, "Not Found");
            };
            let before = st.pr_json(n);
            st.before_write(&format!("pull:{n}"), before);
            if body["state"] == "closed" {
                let was_open = st.prs.get(&n).is_some_and(|p| p.open);
                st.prs.get_mut(&n).expect("pr").open = false;
                if was_open {
                    st.emit_pr("closed", n, json!({}));
                }
            }
            if let Some(t) = body["title"].as_str() {
                st.prs.get_mut(&n).expect("pr").title = t.into();
            }
            reply(200, st.pr_json(n).unwrap_or_default())
        }
        ("GET", ["pulls", n, "files"]) => {
            let Some(pr) = num(n).and_then(|n| st.prs.get(&n).cloned()) else {
                return err(404, "Not Found");
            };
            let tip = st.tip(&pr.base).unwrap_or_default();
            let files = match st.repo.merge_base(&tip, &pr.head_sha) {
                Some(mb) => st.repo.changed_paths(&mb, &pr.head_sha),
                None => vec![],
            };
            let items = files
                .into_iter()
                .map(|f| json!({ "filename": f, "status": "modified" }))
                .collect();
            paginate(base_url, path, query, items, None)
        }
        ("GET", ["pulls", n, "commits"]) => {
            let Some(pr) = num(n).and_then(|n| st.prs.get(&n).cloned()) else {
                return err(404, "Not Found");
            };
            let tip = st.tip(&pr.base).unwrap_or_default();
            let mut items = Vec::new();
            for c in st.repo.first_parent_history(&pr.head_sha) {
                if st.repo.is_ancestor(&c, &tip) {
                    break;
                }
                let commit = st.repo.get(&c);
                items.push(json!({ "sha": c, "commit": { "message": commit.message, "tree": { "sha": commit.tree_id } },
                                   "parents": commit.parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>() }));
            }
            items.reverse();
            paginate(base_url, path, query, items, None)
        }
        ("PUT", ["pulls", n, "merge"]) => {
            let Some(n) = num(n).filter(|n| st.prs.contains_key(n)) else {
                return err(404, "Not Found");
            };
            let pr = st.prs[&n].clone();
            if pr.merged || !pr.open {
                return err(405, "Pull Request is not mergeable");
            }
            if body["sha"].as_str().is_some_and(|s| s != pr.head_sha) {
                return err(
                    409,
                    "Head branch was modified. Review and try the merge again.",
                );
            }
            if body["merge_method"].as_str() != Some("squash") {
                return err(405, "the fake only squashes");
            }
            let tip = st.tip(&pr.base).expect("base");
            let title = body["commit_title"]
                .as_str()
                .unwrap_or(&pr.title)
                .to_string();
            let Ok(c) = st.repo.squash(&tip, &pr.head_sha, &title) else {
                return err(405, "Pull Request is not mergeable");
            };
            if let Some(m) = st.workflow_refusal(&st.repo.changed_paths(&tip, &c)) {
                return err(403, &m);
            }
            let tree = st.repo.tree_id_of(&c);
            let tested_green = st.tree_tested_green(&tree);
            let head_tested = st.head_tested(&pr.head_sha);
            let guard = st.merge_guard.clone().and_then(|g| g(&pr));
            let before = st.pr_json(n);
            st.before_write(&format!("pull:{n}"), before);
            let before = st.ref_json(&pr.base);
            st.before_write(&format!("ref:{}", pr.base), before);
            st.repo.update_ref(&pr.base, &tip, &c).expect("cas");
            let p = st.prs.get_mut(&n).expect("pr");
            p.merged = true;
            p.open = false;
            p.merge_commit_sha = Some(c.clone());
            st.squashes.push(Squash {
                pr: n,
                sha: c.clone(),
                parent: tip.clone(),
                tree,
                tested_green,
                head: pr.head_sha.clone(),
                head_tested,
                guard,
            });
            st.touched(&pr.base);
            st.emit_push(&pr.base, &tip, Some(&c));
            st.emit_pr("closed", n, json!({}));
            reply(
                200,
                json!({ "sha": c, "merged": true, "message": "Pull Request successfully merged" }),
            )
        }
        ("GET", ["git", "ref", "heads", b @ ..]) => {
            let name = b.join("/");
            if let Some(stale) = st.stale_read(&format!("ref:{name}")) {
                return reply(200, stale);
            }
            match st.ref_json(&name) {
                Some(j) => reply(200, j),
                None => err(404, "Not Found"),
            }
        }
        ("GET", ["git", "matching-refs", "heads", p @ ..]) => {
            let prefix = p.join("/");
            let items = st
                .repo
                .refs()
                .iter()
                .filter(|(name, _)| name.starts_with(&prefix))
                .map(|(name, sha)| json!({ "ref": format!("refs/heads/{name}"), "object": { "sha": sha } }))
                .collect();
            paginate(base_url, path, query, items, None)
        }
        ("POST", ["git", "refs"]) => {
            let Some(name) = body["ref"]
                .as_str()
                .and_then(|r| r.strip_prefix("refs/heads/"))
            else {
                return err(422, "ref must start with refs/heads/");
            };
            let sha = body["sha"].as_str().unwrap_or("");
            if st.repo.get_opt(sha).is_none() {
                return err(422, "Object does not exist");
            }
            if st.repo.create_ref(name, sha).is_err() {
                return err(422, "Reference already exists");
            }
            st.emit_push(name, "0000000000000000000000000000000000000000", Some(sha));
            reply(
                201,
                json!({ "ref": format!("refs/heads/{name}"), "object": { "sha": sha } }),
            )
        }
        ("PATCH", ["git", "refs", "heads", b @ ..]) => {
            let name = b.join("/");
            let Some(old) = st.tip(&name) else {
                return err(422, "Reference does not exist");
            };
            let new = body["sha"].as_str().unwrap_or("").to_string();
            if st.repo.get_opt(&new).is_none() {
                return err(422, "Object does not exist");
            }
            let force = body["force"].as_bool().unwrap_or(false);
            if !force && !st.repo.is_ancestor(&old, &new) {
                return err(422, "Update is not a fast forward");
            }
            let before = st.ref_json(&name);
            st.before_write(&format!("ref:{name}"), before);
            st.repo.update_ref(&name, &old, &new).expect("cas");
            st.touched(&name);
            st.emit_push(&name, &old, Some(&new));
            reply(
                200,
                json!({ "ref": format!("refs/heads/{name}"), "object": { "sha": new } }),
            )
        }
        ("DELETE", ["git", "refs", "heads", b @ ..]) => {
            let name = b.join("/");
            let Some(old) = st.tip(&name) else {
                return err(422, "Reference does not exist");
            };
            st.repo.delete_ref(&name);
            st.emit_push(&name, &old, None);
            reply(204, Value::Null)
        }
        ("GET", ["git", "commits", sha]) => match st.repo.get_opt(sha) {
            Some(c) => reply(
                200,
                json!({ "sha": sha, "tree": { "sha": c.tree_id }, "message": c.message,
                        "parents": c.parents.iter().map(|p| json!({ "sha": p })).collect::<Vec<_>>() }),
            ),
            None => err(404, "Not Found"),
        },
        ("POST", ["merges"]) => {
            let base = body["base"].as_str().unwrap_or("").to_string();
            let head = body["head"].as_str().unwrap_or("").to_string();
            let Some(tip) = st.tip(&base) else {
                return err(404, "Base does not exist");
            };
            let head_sha = st.tip(&head).unwrap_or(head);
            if st.repo.get_opt(&head_sha).is_none() {
                return err(404, "Head does not exist");
            }
            if st.repo.is_ancestor(&head_sha, &tip) {
                return reply(204, Value::Null);
            }
            let msg = body["commit_message"]
                .as_str()
                .unwrap_or("Merge")
                .to_string();
            match st.repo.merge(&tip, &head_sha, &msg) {
                Ok(c) => {
                    if let Some(m) = st.workflow_refusal(&st.repo.changed_paths(&tip, &c)) {
                        return err(403, &m);
                    }
                    st.repo.update_ref(&base, &tip, &c).expect("cas");
                    st.queue_commits.insert(c.clone(), base.clone());
                    st.queue_heads
                        .entry(head_sha.clone())
                        .or_default()
                        .insert(base.clone());
                    st.emit_push(&base, &tip, Some(&c));
                    let tree = st.repo.tree_id_of(&c);
                    reply(
                        201,
                        json!({ "sha": c, "commit": { "tree": { "sha": tree }, "message": msg },
                                "parents": [ { "sha": tip }, { "sha": head_sha } ] }),
                    )
                }
                Err(_) => err(409, "Merge conflict"),
            }
        }
        ("GET", ["compare", spec]) => {
            let Some((a, b)) = spec.split_once("...") else {
                return err(404, "Not Found");
            };
            let a = st.tip(a).unwrap_or(a.to_string());
            let b = st.tip(b).unwrap_or(b.to_string());
            if st.repo.get_opt(&a).is_none() || st.repo.get_opt(&b).is_none() {
                return err(404, "Not Found");
            }
            let anc_a = st.repo.ancestors(&a).clone();
            let anc_b = st.repo.ancestors(&b).clone();
            let ahead = anc_b.difference(&anc_a).count();
            let behind = anc_a.difference(&anc_b).count();
            let status = match (ahead, behind) {
                (0, 0) => "identical",
                (_, 0) => "ahead",
                (0, _) => "behind",
                _ => "diverged",
            };
            reply(
                200,
                json!({ "status": status, "ahead_by": ahead, "behind_by": behind }),
            )
        }
        ("GET", ["commits", sha, "check-runs"]) => {
            let runs: Vec<Value> = st
                .runs
                .iter()
                .filter(|r| r.head_sha == *sha)
                .map(|r| st.run_json(r))
                .collect();
            paginate(base_url, path, query, runs, Some("check_runs"))
        }
        ("GET", ["commits", sha, "check-suites"]) => {
            let mut apps: Vec<(String, bool, bool)> = Vec::new();
            for r in st.runs.iter().filter(|r| r.head_sha == *sha) {
                match apps.iter_mut().find(|a| a.0 == r.app_slug) {
                    Some(a) => {
                        a.1 &= r.status == "completed";
                        a.2 &= r.conclusion.as_deref() == Some("success");
                    }
                    None => apps.push((
                        r.app_slug.clone(),
                        r.status == "completed",
                        r.conclusion.as_deref() == Some("success"),
                    )),
                }
            }
            let items = apps
                .into_iter()
                .enumerate()
                .map(|(i, (app, done, ok))| {
                    json!({ "id": i + 1, "head_sha": sha, "app": { "slug": app },
                            "status": if done { "completed" } else { "in_progress" },
                            "conclusion": if done { Some(if ok { "success" } else { "failure" }) } else { None } })
                })
                .collect();
            paginate(base_url, path, query, items, Some("check_suites"))
        }
        ("GET", ["commits", sha, "status"]) => {
            let mut latest: Vec<&crate::FakeStatus> = Vec::new();
            for s in st.statuses.iter().filter(|s| s.sha == *sha) {
                latest.retain(|l| l.context != s.context);
                latest.push(s);
            }
            let state = if latest
                .iter()
                .any(|s| s.state == "failure" || s.state == "error")
            {
                "failure"
            } else if latest.iter().all(|s| s.state == "success") {
                "success"
            } else {
                "pending"
            };
            // Paginated like GitHub: 30 per page unless `per_page` says otherwise.
            let items: Vec<Value> = latest
                .iter()
                .map(|s| json!({ "id": s.id, "context": s.context, "state": s.state }))
                .collect();
            let mut r = paginate(base_url, path, query, items, Some("statuses"));
            r.body["state"] = json!(state);
            r.body["sha"] = json!(sha);
            r
        }
        ("POST", ["check-runs"]) => {
            let sha = body["head_sha"].as_str().unwrap_or("").to_string();
            if st.repo.get_opt(&sha).is_none() {
                return err(422, "No commit found for SHA");
            }
            let id = st.id();
            let app = st.cfg.app_slug.clone();
            st.runs.push(crate::FakeRun {
                id,
                name: body["name"].as_str().unwrap_or("").into(),
                head_sha: sha,
                status: body["status"].as_str().unwrap_or("queued").into(),
                conclusion: body["conclusion"].as_str().map(str::to_string),
                app_slug: app,
                output_title: body["output"]["title"].as_str().unwrap_or("").into(),
            });
            st.emit_run("created", id);
            let r = st.runs.last().cloned().expect("run");
            reply(201, st.run_json(&r))
        }
        ("PATCH", ["check-runs", id]) => {
            let Some(id) = num(id) else {
                return err(404, "Not Found");
            };
            let app = st.cfg.app_slug.clone();
            let Some(r) = st.runs.iter_mut().find(|r| r.id == id && r.app_slug == app) else {
                return err(404, "Not Found");
            };
            if let Some(s) = body["status"].as_str() {
                r.status = s.into();
            }
            if let Some(c) = body["conclusion"].as_str() {
                r.conclusion = Some(c.into());
                r.status = "completed".into();
            }
            if let Some(t) = body["output"]["title"].as_str() {
                r.output_title = t.into();
            }
            let r = r.clone();
            st.emit_run("completed", id);
            reply(200, st.run_json(&r))
        }
        ("GET", ["contents", p @ ..]) => {
            let reference = query_param(query, "ref")
                .map(decode)
                .unwrap_or(st.cfg.default_branch.clone());
            let Some(tip) = st.tip(&reference) else {
                return err(404, "No commit found for the ref");
            };
            match st.repo.tree(&tip).get(&p.join("/")) {
                Some(content) => {
                    let enc = schlange_github::jwt::base64url(content.as_bytes())
                        .replace('-', "+")
                        .replace('_', "/");
                    reply(
                        200,
                        json!({ "type": "file", "encoding": "base64", "content": enc }),
                    )
                }
                None => err(404, "Not Found"),
            }
        }
        ("GET", ["collaborators", user, "permission"]) => reply(
            200,
            json!({ "permission": st.collaborators.get(*user).cloned().unwrap_or("none".into()) }),
        ),
        ("GET", ["rulesets"]) => {
            let items = st
                .rulesets
                .iter()
                .map(|r| json!({ "id": r["id"], "name": r["name"], "target": r["target"], "enforcement": r["enforcement"] }))
                .collect();
            paginate(base_url, path, query, items, None)
        }
        ("GET", ["rulesets", id]) => {
            match st.rulesets.iter().find(|r| r["id"].as_u64() == num(id)) {
                Some(r) => reply(200, r.clone()),
                None => err(404, "Not Found"),
            }
        }
        ("GET", ["rules", "branches", _b @ ..]) => {
            let items = st
                .rulesets
                .iter()
                .map(|r| json!({ "type": "pull_request", "ruleset_id": r["id"] }))
                .collect();
            paginate(base_url, path, query, items, None)
        }
        ("GET", ["branches", _b @ .., "protection"]) => {
            err(403, "Resource not accessible by integration")
        }
        ("POST", ["issues", n, "comments"]) => {
            let Some(n) = num(n).filter(|n| st.prs.contains_key(n)) else {
                return err(404, "Not Found");
            };
            let id = st.id();
            st.comments
                .push((n, id, body["body"].as_str().unwrap_or("").into()));
            reply(201, json!({ "id": id }))
        }
        ("POST", ["issues", "comments", id, "reactions"]) => {
            let Some(id) = num(id).filter(|id| st.comments.iter().any(|c| c.1 == *id)) else {
                return err(404, "Not Found");
            };
            st.reactions
                .push((id, body["content"].as_str().unwrap_or("").into()));
            reply(201, json!({ "id": id, "content": body["content"] }))
        }
        ("POST", ["issues", n, "labels"]) => {
            let Some(n) = num(n).filter(|n| st.prs.contains_key(n)) else {
                return err(404, "Not Found");
            };
            let labels: Vec<String> = body["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            for l in &labels {
                st.prs.get_mut(&n).expect("pr").labels.insert(l.clone());
                st.emit_pr("labeled", n, json!({ "label": { "name": l } }));
            }
            let all: Vec<Value> = st.prs[&n]
                .labels
                .iter()
                .map(|l| json!({ "name": l }))
                .collect();
            reply(200, Value::Array(all))
        }
        ("DELETE", ["issues", n, "labels", label]) => {
            let Some(n) = num(n).filter(|n| st.prs.contains_key(n)) else {
                return err(404, "Not Found");
            };
            if !st.prs.get_mut(&n).expect("pr").labels.remove(*label) {
                return err(404, "Label does not exist");
            }
            st.emit_pr("unlabeled", n, json!({ "label": { "name": label } }));
            reply(200, json!([]))
        }
        _ => err(404, "Not Found"),
    }
}
