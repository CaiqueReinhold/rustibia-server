# Production deployment

Ansible builds the `server` and `site` images on this machine, pushes them to GHCR, and runs
them with Docker Compose on one host behind nginx.

```
            :443 / :80 ─► nginx ─► site:8080 ──┐
internet ─┤                                      ├─► db (postgres, not published)
            :5555 ──────► server ─► site:8443 ──┘   (mTLS)
                             └────► lgtm:4317  ◄── Grafana on 127.0.0.1:3000
```

| Playbook | Runs on | Does |
|---|---|---|
| `build.yml` | this machine | refuses a dirty tree or an LFS-pointer map, builds both images for `linux/amd64`, pushes `ghcr.io/<owner>/rustibia-{server,site}:<git sha>` |
| `provision.yml` | host | Docker from Docker's apt repo, certbot, UFW (ssh, 80, 443, 5555) |
| `deploy.yml` | both | generates the mTLS bundle locally once, copies it, templates `/opt/rustibia/compose.yaml` and nginx, starts the stack, issues the Let's Encrypt certificate on first run |
| `pipeline.yml` | — | all three in order |

## One-time setup

1. `pipx install ansible-core` (or your package manager), then from `deploy/ansible/`:
   `ansible-galaxy collection install -r requirements.yml`.
2. `cp inventory.example.yml inventory.yml` and set the host's address and SSH user.
3. Edit `group_vars/all/vars.yml`: `domain`, `letsencrypt_email`, `ghcr_owner` / `ghcr_user`.
4. Install the 1Password CLI (`op`) and turn on *Settings → Developer → Integrate with
   1Password CLI* in the desktop app. The playbooks read their secrets with `op read` at run
   time, so nothing secret is stored in the repository or on this disk.
5. In 1Password, create a vault `Rustibia` holding an item `deploy` with three fields
   (or point `op_item` in `vars.yml` at another item):
   - `ghcr_push_token` — a GitHub classic PAT with `write:packages`, used here to push.
   - `ghcr_pull_token` — a classic PAT with `read:packages` only, stored on the host to pull.
   - `postgres_password` — alphanumeric only (`openssl rand -hex 32`); it is embedded in a
     connection URL.
   - `grafana_admin_password` — Grafana's `admin` login, re-applied on every deploy.
6. Point the domain's DNS A record at the host. Let's Encrypt validates over port 80 on the
   first deploy, so the record must resolve before then.

## Running

From `deploy/ansible/`:

```bash
ansible-playbook pipeline.yml           # first time, or after host changes
ansible-playbook build.yml deploy.yml   # a normal release
ansible-playbook deploy.yml -e image_tag=<sha>   # roll back to a pushed tag
```

Images are tagged with the 12-character commit sha. `-e allow_dirty=true` builds an
uncommitted tree anyway, tagged `<sha>-dirty`.

Grafana: `ssh -L 3000:127.0.0.1:3000 <host>`, then http://localhost:3000 as `admin` with
`grafana_admin_password`. Change the password in 1Password, not in Grafana: every deploy resets
it to the 1Password value.

## Things to know

- **Docker-published ports bypass UFW.** The firewall only governs what runs on the host
  itself; what the internet can reach in a container is exactly the `ports:` in
  `templates/compose.yaml.j2`. Postgres and the site's 8080/8443 are deliberately unpublished,
  and Grafana is bound to loopback. The otel-lgtm image turns on anonymous **Admin** access by
  default; the compose file turns it off, because loopback does not keep out the other containers.
- **The site trusts `X-Forwarded-For`** for rate limiting, which is safe only while nginx is
  its sole way in and overwrites that header. Do not publish the site's port.
- **`deploy/certs/` holds the internal CA key.** It is git-ignored and never copied to the
  host (only `ca.crt` and the two leaves are). Deleting it regenerates the bundle on the next
  deploy, which is fine — both sides are replaced together.
- **Rotating a secret** is an edit in 1Password followed by `ansible-playbook deploy.yml`.
  The exception is `postgres_password`, which only takes effect when the volume is first
  created: change it with `ALTER USER` inside the database first, then in 1Password.
- **Neither process handles SIGTERM.** A deploy that recreates the server kills it outright, so
  players lose progress since their last periodic save (up to `save_interval`, 60 s).
- **The server holds about 2.5 GB resident** with the full spawn table loaded, before any
  player connects; lgtm adds roughly another gigabyte. Size the host for both.
- Certificates renew via the certbot package's own systemd timer; the deploy hook in
  `/etc/letsencrypt/renewal-hooks/deploy/` reloads nginx.
