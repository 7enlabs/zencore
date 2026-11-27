# browser-cdp-spike

Spike cho `docs/browser-agent-spec.md` (Chặng 1, spec + spike, không implement tính năng).

Chứng minh từ Rust lái headless Chromium qua CDP thô (WebSocket, `tokio-tungstenite`
0.26.2): navigate tới trang công khai có JS, đọc text sau khi JS chạy, chụp
screenshot PNG thật. Không credential, không đăng nhập.

## Chạy

```sh
cargo run --release [-- <url>] [--out <png path>]
```

Mặc định: url `https://quotes.toscrape.com/js/`, ảnh `./screenshot.png`.

## Môi trường đã kiểm chứng (box `zencode`, 27/11/2026)

- Chromium hệ thống `155.0.8059.79` (gói Debian `155.0.8059.79-1~deb13u1`,
  `sudo apt install chromium`), CDP protocol 1.3, V8 15.5.35.23.
- cargo 1.99.0, rustc 1.99.0.
- Crate độc lập (`[workspace]` riêng), không thuộc workspace ZenCore.

## Kết quả chạy

Xem `RUN_LOG.md` (log chạy thật trên box, gồm đường dẫn + kích thước screenshot).
