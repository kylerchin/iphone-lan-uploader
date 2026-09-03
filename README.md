# iPhone LAN Uploader

A tiny LAN-only uploader for getting photos/videos off an old iPhone through Mobile Safari.

## Run

```sh
cargo run --release
```

Then open the printed URL on the iPhone, for example:

```text
http://192.168.1.23:8787/
```

## Optional settings

```sh
PORT=8787 UPLOAD_DIR=iphone_uploads MAX_GB=100 cargo run --release
```

- `PORT`: HTTP port to listen on.
- `UPLOAD_DIR`: where uploaded files are saved.
- `MAX_GB`: maximum total size of one HTTP request.

## Notes

- Desktop and iPhone must be on the same Wi-Fi/LAN.
- Allow the app through your desktop firewall when prompted.
- Upload large camera rolls in batches; old Safari is more reliable that way.
- This is plain HTTP for trusted local networks only.
