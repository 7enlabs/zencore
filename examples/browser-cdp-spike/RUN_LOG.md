# RUN_LOG — browser-cdp-spike

Chạy thật trên box `zencode`, 27/11/2026 ~20:40 Asia/Saigon (13:40 UTC).

## Lệnh

```sh
cd examples/browser-cdp-spike
cargo build --release
./target/release/browser-cdp-spike
```

## Kết quả build

- `Finished release profile [optimized] target(s) in 29.73s`, exit 0.
- Lần build lại sau sửa code: 2.91s.

## Kết quả chạy (exit 0)

```text
chromium: /usr/bin/chromium (Chromium 155.0.8059.79 built on Debian GNU/Linux 13 (trixie))
launched headless Chromium, remote-debugging-port=40139
CDP: browser=Chrome/155.0.8059.79 protocol=1.3 v8=15.5.35.23
navigated to https://quotes.toscrape.com/js/
page load event fired
diagnostics: readyState="complete" jQuery="function" innerText_len=1499
JS-rendered quotes found: 10
--- innerText (first 500 chars) ---
Quotes to Scrape

Login

"The world as we have created it is a process of our thinking. It cannot be changed without changing our thinking."
by Albert Einstein
Tags: change deep-thoughts thinking world
"It is our choices, Harry, that show what we truly are, far more than our abilities."
by J.K. Rowling
Tags: abilities choices
"There are only two ways to live your life. One is as though nothing is a miracle. The other is as though everything is a miracle."
by Albert Einstein
Tags: inspirational l
--- end innerText, total 1499 chars ---
screenshot: screenshot.png (75878 bytes)
OK: navigate + JS text + screenshot all worked
```

## Screenshot thật

- Đường dẫn (trong repo): `examples/browser-cdp-spike/screenshot.png`
- Kích thước: **75878 bytes**, PNG 1280x713, 8-bit/color RGB
- SHA256: `e03d93385e708c609110fb4f9c6380f656b0478a81e7e842467b44feb1f6b33d`
- Kiểm chứng nội dung (decode PNG bằng Python stdlib, không dùng thư viện ngoài):
  10098 pixel mẫu, 184 màu khác nhau, 760 pixel không phải trắng — ảnh render thật,
  không phải trang trắng.
- Đối chiếu: HTML thô của trang (5808 bytes, `curl`) không chứa element `.quote` nào
  được render — chuỗi `class='quote'` duy nhất nằm trong JS `document.write`
  (dòng 167). Đếm được 10 `.quote` sau khi load = JS đã chạy thật.

## Lỗi gặp trong lúc làm spike (đã sửa, ghi để khỏi lặp lại)

1. Lần chạy đầu: `JS-rendered quotes found: 0`. Nguyên nhân là bug trong `eval()`:
   đọc `result.result.result.value` thừa một tầng (`call()` đã trả `msg["result"]`).
   Sửa thành `result.result.value` thì đếm được 10 quote.
2. `grep -c "class=.quote."` trên HTML thô trả 1 — đó là chuỗi JS, không phải element
   đã render (đã đối chiếu ở trên).
