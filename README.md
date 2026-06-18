# cftun

A tiny Rust CLI that turns Cloudflare Tunnel into a free, persistent ngrok alternative for webhooks.

Set it up once. Run one command. Your webhook URL never changes.

## Why

ngrok's free tier gives you a random URL every restart. That means updating Stripe, Lemon Squeezy, GitHub, or whatever dashboard every time you start coding.

If you already have a domain on Cloudflare, **cloudflared** can do the same thing for free — with a fixed subdomain. This CLI just removes the boilerplate. Based on this [blog post](https://www.carlo.tl/blog/cloudflared-ngrok-alternative-for-testing-webhooks) I wrote.

## Install

```bash
cargo install --path .
```

Requires `cloudflared` installed and logged in:

```bash
brew install cloudflared
cloudflared tunnel login
```

## Quick start

```bash
# Create a persistent tunnel
cftun create my-tunnel webhook.example.com 3000

# Run it
cftun run my-tunnel
```

Your webhook URL is now `https://webhook.example.com` and it stays that way every time you run it.

## Commands

| Command                                              | Description                                                   |
| ---------------------------------------------------- | ------------------------------------------------------------- |
| `cftun create <name> <hostname> <port/url>`          | Create a tunnel, route DNS, and write the config              |
| `cftun list` / `cftun ls`                            | Show managed tunnels + other cloudflared tunnels              |
| `cftun run <name>`                                   | Start the tunnel                                              |
| `cftun show <name>`                                  | Print the config file for a tunnel                            |
| `cftun update <name> --hostname <new> --local <new>` | Update the hostname or local target                           |
| `cftun import <name> <hostname> <port/url>`          | Adopt an existing cloudflared tunnel into cftun               |
| `cftun status`                                       | Show all cloudflared tunnels with connection status           |
| `cftun delete <name> [--cleanup]`                    | Remove from cftun metadata, optionally delete from Cloudflare |

## Local URL formats

```bash
cftun create my-tunnel webhook.example.com 3000          # http://localhost:3000
cftun create my-tunnel webhook.example.com 443           # https://localhost:443
cftun create my-tunnel webhook.example.com https://localhost:3000
```

## How it works

- Each tunnel gets its own config at `~/.cloudflared/cftun/<name>.yaml`
- cftun tracks metadata at `~/.cloudflared/cftun/tunnels.yaml`
- It runs `cloudflared tunnel --config <file> run` under the hood
- DNS routes are handled via `cloudflared tunnel route dns`

## License

MIT
