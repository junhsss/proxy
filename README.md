# HTTP Proxy server

## Usage

Start the server:
```bash
cargo run --release
```

The server will start on `127.0.0.1:3000` by default.

### Authentication

Default credentials:

- Username: `admin`
- Password: `secret`

### Example Usage

```bash
# HTTP requests
curl -x http://127.0.0.1:3000 --proxy-user admin:secret -L http://example.com

# HTTPS requests
curl -x http://127.0.0.1:3000 --proxy-user admin:secret -L https://example.com
```

Using the `/metrics` endpoint

```bash
curl http://127.0.0.1:3000/metrics
```
