---
id: integrations-account
title: "Integrations, products and account"
priority: P3
arztagent: false
status: missing
inventory:
  - "api:*/integrations/*"
  - "api:GET /default_products/{owner}"
  - "api:PUT /default_products/{owner}"
  - "api:PUT /products/{owner}/{repository}"
  - "api:GET /application"
  - "cli:mergify auth *"
problems:
  - "W-20"
source: "https://docs.mergify.com/integrations/slack"
---

# Integrations, products and account

Slack notifications, enabling products, application keys and CLI login.

## Sub-features

- `slack`
- `products`
- `auth`

## How to get to it (user POV)

- Dashboard or CLI.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not planned.** No verification path; the feature stays `missing` on purpose.

## Gotchas

- –
