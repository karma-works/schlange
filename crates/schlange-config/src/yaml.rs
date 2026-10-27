//! Stage 1: YAML. Parses a document into a JSON value and records the position of every node,
//! keyed by JSON pointer, so that later stages can report line and column.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::Marker;

/// 1-based line and column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl From<Marker> for Position {
    fn from(m: Marker) -> Self {
        Position {
            line: m.line(),
            column: m.col() + 1,
        }
    }
}

/// Positions of the nodes of one document.
#[derive(Debug, Default)]
pub struct Spans {
    values: HashMap<String, Position>,
    keys: HashMap<String, Position>,
}

impl Spans {
    /// Position of the value at `pointer`, or of its nearest ancestor that has one
    /// (for example inside an alias).
    pub fn value(&self, pointer: &str) -> Option<Position> {
        let mut p = pointer;
        loop {
            if let Some(pos) = self.values.get(p) {
                return Some(*pos);
            }
            match p.rfind('/') {
                Some(i) => p = &p[..i],
                None => return self.values.get("").copied(),
            }
        }
    }

    /// Position of the key that holds the value at `pointer`; falls back to the value.
    pub fn key(&self, pointer: &str) -> Option<Position> {
        self.keys
            .get(pointer)
            .copied()
            .or_else(|| self.value(pointer))
    }
}

/// A problem found while reading YAML.
#[derive(Debug, Clone)]
pub struct YamlError {
    pub pointer: String,
    pub position: Option<Position>,
    pub message: String,
}

/// Escapes one JSON pointer segment (RFC 6901).
pub fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

enum Frame {
    Mapping {
        pointer: String,
        pending_key: Option<(String, Position)>,
        seen: HashSet<String>,
    },
    Sequence {
        pointer: String,
        index: usize,
    },
}

#[derive(Default)]
struct Recorder {
    stack: Vec<Frame>,
    spans: Spans,
    errors: Vec<YamlError>,
    /// Depth of nested collections inside a complex (non-scalar) key; ignored.
    skip: usize,
}

impl Recorder {
    /// Returns the pointer of the next value, or `None` when the event is a mapping key.
    fn next_slot(&mut self, scalar_key: Option<&str>, mark: Marker) -> Option<String> {
        match self.stack.last_mut() {
            None => Some(String::new()),
            Some(Frame::Sequence { pointer, index }) => {
                let p = format!("{pointer}/{index}");
                *index += 1;
                Some(p)
            }
            Some(Frame::Mapping {
                pointer,
                pending_key,
                seen,
                ..
            }) => match pending_key.take() {
                Some((key, _)) => Some(format!("{pointer}/{}", escape(&key))),
                None => {
                    let key = scalar_key.unwrap_or("").to_string();
                    let child = format!("{pointer}/{}", escape(&key));
                    let pos = Position::from(mark);
                    if scalar_key.is_some() && !seen.insert(key.clone()) {
                        self.errors.push(YamlError {
                            pointer: child.clone(),
                            position: Some(pos),
                            message: format!("duplicate key `{key}`"),
                        });
                    }
                    self.spans.keys.insert(child, pos);
                    *pending_key = Some((key, pos));
                    None
                }
            },
        }
    }

    fn mapping_wants_key(&self) -> bool {
        matches!(
            self.stack.last(),
            Some(Frame::Mapping {
                pending_key: None,
                ..
            })
        )
    }
}

impl MarkedEventReceiver for Recorder {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        if self.skip > 0 {
            match ev {
                Event::MappingStart(..) | Event::SequenceStart(..) => self.skip += 1,
                Event::MappingEnd | Event::SequenceEnd => {
                    self.skip -= 1;
                    if self.skip == 0 {
                        // The complex key is complete; its value follows.
                        if let Some(Frame::Mapping { pending_key, .. }) = self.stack.last_mut() {
                            *pending_key = Some(("?".into(), Position::from(mark)));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        match ev {
            Event::Scalar(ref value, ..) => {
                if let Some(p) = self.next_slot(Some(value), mark) {
                    self.spans.values.insert(p, Position::from(mark));
                }
            }
            Event::Alias(_) => {
                if let Some(p) = self.next_slot(Some("<<alias>>"), mark) {
                    self.spans.values.insert(p, Position::from(mark));
                }
            }
            Event::MappingStart(..) | Event::SequenceStart(..) => {
                if self.mapping_wants_key() {
                    // A collection used as a key: not valid for Mergify, skip it.
                    self.skip = 1;
                    return;
                }
                let p = self.next_slot(None, mark).unwrap_or_default();
                self.spans.values.insert(p.clone(), Position::from(mark));
                let frame = if matches!(ev, Event::MappingStart(..)) {
                    Frame::Mapping {
                        pointer: p,
                        pending_key: None,
                        seen: HashSet::new(),
                    }
                } else {
                    Frame::Sequence {
                        pointer: p,
                        index: 0,
                    }
                };
                self.stack.push(frame);
            }
            Event::MappingEnd | Event::SequenceEnd => {
                self.stack.pop();
            }
            _ => {}
        }
    }
}

/// Parses one YAML document. On success returns the JSON value (merge keys `<<` applied) and
/// the positions; otherwise every YAML error found.
pub fn parse(source: &str) -> Result<(Value, Spans), Vec<YamlError>> {
    let mut recorder = Recorder::default();
    let mut parser = Parser::new_from_str(source);
    if let Err(e) = parser.load(&mut recorder, false) {
        return Err(vec![YamlError {
            pointer: String::new(),
            position: Some(Position::from(*e.marker())),
            message: format!("YAML syntax error: {}", e.info()),
        }]);
    }
    if !recorder.errors.is_empty() {
        return Err(recorder.errors);
    }
    let mut value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(source) {
        Ok(v) => v,
        Err(e) => {
            let position = e.location().map(|l| Position {
                line: l.line(),
                column: l.column(),
            });
            return Err(vec![YamlError {
                pointer: String::new(),
                position,
                message: format!("YAML error: {e}"),
            }]);
        }
    };
    if let Err(e) = value.apply_merge() {
        return Err(vec![YamlError {
            pointer: String::new(),
            position: None,
            message: format!("YAML merge key error: {e}"),
        }]);
    }
    let json = serde_json::to_value(&value).map_err(|e| {
        vec![YamlError {
            pointer: String::new(),
            position: None,
            message: format!("YAML value cannot be represented as JSON: {e}"),
        }]
    })?;
    let json = if json.is_null() {
        Value::Object(Default::default())
    } else {
        json
    };
    Ok((json, recorder.spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_positions() {
        let src = "queue_rules:\n  - name: default\n    merge_method: squash\n";
        let (v, spans) = parse(src).unwrap();
        assert_eq!(v["queue_rules"][0]["merge_method"], "squash");
        assert_eq!(
            spans.value("/queue_rules/0/merge_method"),
            Some(Position {
                line: 3,
                column: 19
            })
        );
        assert_eq!(
            spans.key("/queue_rules/0/merge_method"),
            Some(Position { line: 3, column: 5 })
        );
        // Unknown pointers fall back to the nearest ancestor.
        assert_eq!(
            spans.value("/queue_rules/0/merge_method/x"),
            Some(Position {
                line: 3,
                column: 19
            })
        );
    }

    #[test]
    fn duplicate_key() {
        let errs = parse("a: 1\nb: 2\na: 3\n").unwrap_err();
        assert_eq!(errs[0].message, "duplicate key `a`");
        assert_eq!(errs[0].position, Some(Position { line: 3, column: 1 }));
    }

    #[test]
    fn syntax_error_has_position() {
        let errs = parse("a: [1, 2\nb: 3\n").unwrap_err();
        assert!(errs[0].message.starts_with("YAML syntax error"));
        assert!(errs[0].position.is_some());
    }

    #[test]
    fn anchors_and_merge_keys() {
        let src =
            "shared:\n  base: &base\n    - base = main\nx: &m\n  a: 1\ny:\n  <<: *m\n  b: 2\n";
        let (v, _) = parse(src).unwrap();
        assert_eq!(v["y"]["a"], 1);
        assert_eq!(v["y"]["b"], 2);
    }
}
