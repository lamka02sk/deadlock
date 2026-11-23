Keys for the `ssh` test server in `docker-compose.yml` only. They guard nothing.

- `id_ed25519`: authorized, no passphrase.
- `id_ed25519_pass`: authorized, passphrase `deadlock`.
- `wrong_ed25519`: not authorized.
