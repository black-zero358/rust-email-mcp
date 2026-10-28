# Security

Report a vulnerability through GitHub's private vulnerability reporting when enabled, or contact the repository owner through their public GitHub profile. Include the affected commit, expected behavior and a minimal reproduction using synthetic data.

Never attach credentials, tokens, mail contents, private keys or production databases. Keep exploit details private until a fix is available.

The service validates OIDC access tokens on every request and binds mailbox access to configured subjects, audiences and scopes. Keep access-token lifetimes short. Rotate mailbox credentials and OIDC signing keys through their owning services.

Run behind HTTPS, bind container ports to loopback, restrict file permissions and keep data backups encrypted. Deploy one process per operation database. Retain operation keys for as long as duplicate-send protection is required.
