# Browser Agent — Spec (Chặng 1)

- Trạng thái: **spec + spike, chưa implement**. Leader review spec rồi mới giao implement.
- Nhánh: `dev/browser-agent-spec`, base `origin/main` tại `6986330` (27/11/2026).
- Giao việc: ZC Leader tin Telegram 25353, 13:13:20 UTC 27/11/2026.
- Thứ tự Zen đã chốt: **browser agent trước, computer-use sau**.
- Spike kèm theo: `examples/browser-cdp-spike/` (chạy được trên box, xem mục 8).
- Sửa sau review: Z Tester review độc lập (tin Telegram 25382), Leader giao sửa
  (tin 25385) — fmt spike, chạy lại spike, ghi rõ Chromium, bổ sung kiểm soát
  mạng toàn phiên + duyệt riêng submit/phá hủy + serialize selector an toàn.

## 1. Hiện trạng

ZenCore hiện chỉ có WebFetch/WebSearch đọc một chiều (`crates/zen-tools/src/builtin.rs`:
`WebFetchTool` ở dòng 1471, `WebSearchTool` ở dòng 1619). Cả hai đều không thao tác
được trên web: không click, không điền form, không chạy JS, không đăng nhập, không đọc
nội dung do JS render. WebFetch dùng reqwest, timeout 10s, cap 5MB, chặn private network
qua `check_fetch_target`, chỉ nhận `http(s)`.

Hướng đi: lái headless Chromium qua CDP (Chrome DevTools Protocol) làm tool trong
agent loop. Một browser session sống trong phiên agent; mỗi tool call là một hoặc
nhiều lệnh CDP.

## 2. Mặt tool

Sáu tool mới, đặt trong `zen-tools` cạnh WebFetch/WebSearch, cùng implement trait
`zen_core::Tool` (`name`, `description`, `parameters_schema`, `is_read_only`, `call` —
`crates/zen-core/src/lib.rs`, dòng ~701). Quy ước tên theo PascalCase như tool hiện có.

### 2.1 `browser_navigate` — đọc (read-only)

| Tham số | Kiểu | Bắt buộc | Mô tả |
|---|---|---|---|
| `url` | string | có | URL tuyệt đối `http(s)`. Nâng `http` lên `https` như WebFetch (`upgrade_scheme`). |
| `wait_until` | string | không | `load` (mặc định) hoặc `networkidle`. Hết `load` thì trả ngay; `networkidle` chờ thêm tới khi 500ms không còn request mạng. |
| `timeout_ms` | integer | không | Mặc định 30000, tối đa 120000. |

Trả về: URL cuối sau redirect, HTTP status, `title`, độ dài `innerText`. Mỗi hop
redirect đều kiểm tra lại bằng `check_fetch_target` như URL ban đầu (mục 6,
điểm 9): redirect tới mạng riêng thì chặn, không navigate tiếp. Lỗi: URL
không hợp lệ, timeout, trang trả lỗi HTTP, target đóng.

### 2.2 `browser_click` — đổi trạng thái (mutating)

| Tham số | Kiểu | Bắt buộc | Mô tả |
|---|---|---|---|
| `selector` | string | có | CSS selector, duy nhất một element. |
| `timeout_ms` | integer | không | Mặc định 10000. |

Luồng: `Runtime.evaluate` tìm element (`document.querySelector`), cuộn vào viewport,
`Input.dispatchMouseEvent` click tại tâm bounding box. Trả về: selector, text của
element đã click, URL sau click (nếu điều hướng). Lỗi: không tìm thấy element, element
không nhìn thấy/không click được, timeout. Không click element `type=password` hay
nút submit của form đăng nhập nếu chưa qua cổng quyền đăng nhập (mục 5).

### 2.3 `browser_type` — đổi trạng thái (mutating)

| Tham số | Kiểu | Bắt buộc | Mô tả |
|---|---|---|---|
| `selector` | string | có | CSS selector của `input`/`textarea`/contenteditable. |
| `text` | string | có | Văn bản cần gõ. |
| `submit` | boolean | không | Mặc định `false`. `true` = nhấn Enter sau khi gõ. |
| `timeout_ms` | integer | không | Mặc định 10000. |

Luồng: focus element, `Input.insertText` (không gõ từng phím trừ khi trang yêu cầu
sự kiện phím — lúc đó dùng `Input.dispatchKeyEvent`). Trả về: selector, độ dài đã gõ
(không echo lại nội dung). **Không bao giờ log hay trả về nội dung đã gõ khi
`type=password` hoặc khi selector trỏ field mật khẩu** (mục 6).

### 2.4 `browser_content` — đọc (read-only)

| Tham số | Kiểu | Bắt buộc | Mô tả |
|---|---|---|---|
| `selector` | string | không | Không có = toàn bộ `document.body.innerText`. Có = innerText của element đó. |
| `max_chars` | integer | không | Mặc định 20000, tối đa 100000. Vượt thì cắt và ghi rõ đã cắt. |

Trả về text sau khi JS đã chạy (khác WebFetch ở đúng điểm này). Lỗi: selector không
tìm thấy.

### 2.5 `browser_screenshot` — đọc (read-only)

| Tham số | Kiểu | Bắt buộc | Mô tả |
|---|---|---|---|
| `selector` | string | không | Không có = chụp viewport. Có = chụp đúng bounding box element. |
| `full_page` | boolean | không | Mặc định `false`. `true` = chụp toàn trang (`captureBeyondViewport`). |

Trả về: đường dẫn file PNG đã lưu trong thư mục làm việc của phiên (không nhúng
base64 vào tool result — ảnh lớn làm tràn context), kèm kích thước file và kích
thước viewport. Lỗi: target đóng, element không thấy.

### 2.6 `browser_wait` — đọc (read-only)

| Tham số | Kiểu | Bắt buộc | Mô tả |
|---|---|---|---|
| `selector` | string | không | Chờ element xuất hiện. Không có thì chờ theo `timeout_ms`. |
| `text` | string | không | Chờ text xuất hiện trong `innerText`. Chỉ dùng một trong hai: `selector` hoặc `text`. |
| `timeout_ms` | integer | không | Mặc định 10000, tối đa 60000. |

Trả về: điều kiện nào đã thỏa, thời gian chờ thực tế. Timeout thì trả lỗi rõ ràng
(chứ không im lặng trả trang cũ).

### 2.7 Quy tắc chung

- Một session = một browser + một tab mặc định. Tool call đầu tiên (`browser_navigate`
  hoặc bất kỳ tool nào) khởi tạo session lười (lazy); session đóng khi phiên agent kết thúc.
- Mỗi tool call có `timeout_ms` riêng; quá hạn thì hủy lệnh CDP đang chờ, không để
  agent treo.
- Mọi lỗi CDP trả về dạng `Error: <tool>: <lý do>` như quy ước của WebFetch, không panic.
- Selector không hợp lệ (CSS sai cú pháp) trả lỗi ngay, không gửi xuống CDP.

## 3. Client CDP: chọn CDP thô qua WebSocket

### 3.1 Các lựa chọn đã đo thật trên box `zencode` ngày 27/11/2026

Box: cargo 1.99.0, rustc 1.99.0, Chromium hệ thống 155.0.8059.79 (xem mục 8).
`cargo fetch` tải được từ crates.io (API `crates.io` trả 403 nhưng `index.crates.io`
và `static.crates.io` trả 200 — fetch vẫn chạy).

| Tiêu chí | A. CDP thô qua `tokio-tungstenite` | B. `chromiumoxide` 0.7.0 | C. `headless_chrome` 1.x |
|---|---|---|---|
| Phiên bản đo được | `tokio-tungstenite 0.26.2` + `tungstenite 0.26.2` | `0.7.0` (+ `chromiumoxide_cdp 0.7.0`, `_pdl 0.7.0`, `_types 0.7.0`) | `1.0.22` (resolve được trên box) |
| Số package trong `Cargo.lock` của project thử | 65 (probe `/tmp/cdp-probe`) | 190 (probe `/tmp/cdp-probe2`, lock 1847 dòng) | 139 (probe `/tmp/cdp-probe3`) |
| Runtime | tokio — trùng runtime ZenCore đã dùng | async-std mặc định; muốn tokio phải bật feature `tokio` (docs.rs: "By default chromiumoxide is configured with async-std") | đồng bộ (blocking), tự spawn thread + channel |
| API | Tự viết message JSON theo protocol 1.3; chỉ dùng domain cần: `Page`, `Runtime`, `Input` | API cao cấp, typed toàn bộ CDP (code generate từ PDL trong build.rs của `chromiumoxide_cdp`) | API đồng bộ đơn giản (`Browser::default`, `new_tab`) |
| Browser binary | Dùng Chromium hệ thống, không tải thêm | Mặc định tìm Chromium đã cài; có `fetcher` tải bản riêng nếu bật feature (docs.rs) | Tự tìm/tải Chromium nếu thiếu |
| Bảo trì | `tokio-tungstenite`/`tungstenite` còn được bảo trì tích cực | Còn bảo trì (0.7.0 mới nhất tại thời điểm đo); có fork `spider_chromiumoxide_cdp 0.7.1` | Ít cập nhật lớn gần đây; dev.to nhận xét "less maintained" |
| Rủi ro | Tự chịu trách nhiệm parse event, match id | Build nặng (codegen PDL), API đổi theo major; handler task phải poll liên tục (`Browser::launch` trả `(browser, handler)`) | API đồng bộ chặn thread async của agent; ít typed CDP hơn B |

Nguồn: `docs.rs/crate/chromiumoxide_cdp/0.7.0` (mô tả API, fetcher, generated code),
`cargo tree -p chromiumoxide` chạy trên box, `cargo fetch` cả ba lựa chọn.

### 3.2 Kết luận: chọn A — CDP thô qua `tokio-tungstenite`

- Trùng stack sẵn có: ZenCore đã dùng tokio 1.x + serde_json; chỉ thêm 2 crate nhỏ
  (`tokio-tungstenite 0.26.2`, `tungstenite 0.26.2`), không thêm runtime thứ hai.
- Không tải browser ngoài: dùng đúng Chromium hệ thống đã kiểm chứng (155.0.8059.79),
  tránh fetcher tải binary lạ về box — khớp luật free-only và chính sách an toàn.
- Đủ cho 6 tool ở mục 2: chỉ cần 3 domain CDP (`Page.navigate`/`Page.captureScreenshot`,
  `Runtime.evaluate`, `Input.dispatchMouseEvent`/`insertText`). Typed toàn bộ CDP
  là thừa ở chặng này.
- Spike ở `examples/browser-cdp-spike/` chứng minh A chạy được thật (mục 8).
- Dự phòng: nếu sau này cần typed CDP rộng (network interception, console log...),
  chuyển sang B (`chromiumoxide` 0.7.0, feature `tokio`) — interface tool ở mục 2
  không đổi, chỉ thay lớp client bên dưới.

### 3.3 Thiết kế client (implement sau)

- Module mới, ví dụ `crates/zen-tools/src/browser.rs` (hoặc crate riêng nếu
  Leader muốn tách): struct `BrowserSession` giữ `Child` (tiến trình Chromium),
  `WebSocketStream`, `next_id`, buffer event.
- Khởi động: tìm binary theo thứ tự `CHROMIUM_BIN` (env) → `which chromium` →
  `which google-chrome`; cờ: `--headless=new --no-sandbox --disable-gpu
  --disable-dev-shm-usage --remote-debugging-port=<port trống> --user-data-dir=<temp riêng>`.
  Port trống lấy bằng cách bind `127.0.0.1:0` rồi thả.
- Bắt tay HTTP: poll `GET /json/version` tới khi 200 (tối đa 30s) → `PUT /json/new`
  tạo target → lấy `webSocketDebuggerUrl` → nối WebSocket.
- Gửi lệnh: JSON `{id, method, params}`; chờ message cùng `id`; event (`method`, không
  có `id`) xếp vào buffer để `browser_wait` và chờ `Page.loadEventFired` đọc sau.
- Dọn dẹp: đóng WebSocket, kill tiến trình, xóa `--user-data-dir` tạm khi session kết thúc.
- Một session chỉ nói chuyện với `127.0.0.1`; không mở remote-debugging ra ngoài.

### 3.4 Serialize selector an toàn khi gọi `Runtime.evaluate`

- **Cấm ghép chuỗi JS**: không bao giờ làm
  `format!("document.querySelector('{selector}')")` hay nối chuỗi tương tự với
  selector/text do agent cung cấp — selector chứa `'` sẽ phá cú pháp, và dữ liệu
  agent có thể thành mã JS tùy ý chạy trên trang.
- **Cách đúng (duy nhất)**: truyền dữ liệu qua tham số JSON của CDP, để JSON
  encoder lo việc escape:
  - `Runtime.callFunctionOn` với `functionDeclaration` là chuỗi cố định trong code
    (`function(sel) { return document.querySelector(sel); }`, không chứa dữ liệu
    agent) và `arguments: [{"value": selector}]` — selector đi trong JSON value,
    không đi trong mã.
  - Hoặc `Runtime.evaluate` với `expression` là template cố định + chuỗi literal
    đã JSON-encode bằng `serde_json::to_string(selector)` (tự escape quote,
    backslash, dấu xuống dòng), không dùng `format!` trực tiếp lên dữ liệu thô.
- **Kiểm tra selector trước**: validate cú pháp CSS selector ở phía Rust trước khi
  gửi xuống CDP (selector sai thì trả lỗi ngay, mục 2.7); `querySelector` trả `null`
  thì báo "không tìm thấy element", không đánh giá tiếp.
- **Text gõ vào cũng là dữ liệu**: `browser_type` truyền text qua `Input.insertText`
  (`text` là tham số JSON), không nhúng text vào `Runtime.evaluate`.
- Spike `examples/browser-cdp-spike/` hiện chỉ evaluate chuỗi cố định trong code
  (`document.querySelectorAll('.quote').length`, `document.readyState`, ...) nên
  không mắc lỗi này; quy tắc trên bắt buộc khi implement tool thật nhận selector
  từ agent.

## 4. Mô hình permissions: nối vào cổng permissions đang làm

Cổng hiện có: `PermissionMode` 6 chế độ (`crates/zen-core/src/lib.rs` dòng 347):
`Default`, `AcceptEdits`, `Plan`, `BypassPermissions`, `DontAsk`, `AskAll`. ACP chỉ
quảng cáo 4 chế độ (`Default`, `Plan`, `AcceptEdits`, `AskAll`); `bypassPermissions`
và `dontAsk` không bao giờ được quảng cáo. Quyết định theo `RulePermissions`
(`crates/zen-tools/src/permissions.rs`): `allow` / `ask` / `deny` + `is_edit_tool`
(dòng 259) + `tool_is_read_only` (dòng 2772+ của `builtin.rs`, hiện liệt kê
`WebFetch`, `WebSearch` là read-only).

Phân loại 6 tool mới:

| Tool | `is_read_only` | Mặc định (mode `Default`) | Lý do |
|---|---|---|---|
| `browser_navigate` | true | cho phép (allow) | Chỉ đọc trang công khai; chặn private network như WebFetch |
| `browser_content` | true | cho phép (allow) | Chỉ đọc text đã render |
| `browser_screenshot` | true | cho phép (allow) | Chỉ ghi file PNG trong thư mục phiên |
| `browser_wait` | true | cho phép (allow) | Chỉ chờ, không đổi trạng thái |
| `browser_click` | false | hỏi (ask) ở mọi mode, không tự cho phép | Có thể kích hoạt hành động trên trang (mua, xóa, gửi); submit hoặc hành động phá hủy cần duyệt riêng (điểm 9) |
| `browser_type` | false | hỏi (ask) ở mọi mode, không tự cho phép | Đưa dữ liệu vào trang; đặc biệt nhạy với field mật khẩu; submit cần duyệt riêng (điểm 9) |

Quy tắc chi tiết:

1. **Mode `Plan`**: mọi tool browser (kể cả read-only) bị chặn — plan mode không cho
   chạy tool đổi trạng thái hay đọc ngoài, giống quy tắc ở `permissions.rs` dòng ~735.
2. **Mode `AskAll`**: mọi tool browser đều hỏi, kể cả read-only.
3. **Mode `AcceptEdits`**: `browser_click`/`browser_type` KHÔNG được tự cho phép —
   vẫn hỏi như ở mode `Default` (điểm 9 dưới đây). `AcceptEdits` chỉ miễn hỏi cho
   thao tác sửa file, không áp cho thao tác trên trình duyệt; thao tác đăng nhập
   vẫn qua gate riêng (điểm 5 dưới đây).
4. **`BypassPermissions` / `DontAsk`**: không bao giờ tự động cho phép
   `browser_click`/`browser_type` — kể cả khi mode này bỏ qua hỏi cho tool khác;
   đăng nhập hay submit form có credential cũng vậy. Các tool browser khác
   (read-only) theo luật chung của cổng.
5. **Rule gợi ý** (`suggested_rule`/`suggested_rules` trong `permissions.rs`):
   thêm `BrowserClick`, `BrowserType`, `BrowserLogin` để người dùng ghi rule
   `allow`/`ask`/`deny` trong settings, ví dụ `deny BrowserLogin` chặn mọi đăng nhập.
6. **Mở rộng `is_edit_tool`**: thêm `browser_click`, `browser_type` vào nhóm edit
   để ACP prompter map đúng `session/request_permission` (hiện chỉ có
   `Edit|Write|MultiEdit|NotebookEdit`).
7. **Theo origin**: rule `ask`/`deny` có thể gắn theo host (ví dụ chỉ hỏi khi click
   trên host lạ, allow khi click trên host đã duyệt). Implement ghi rõ host trong
   prompt xin quyền: tool, selector, host, hành động.
8. **Audit**: mỗi lần tool browser chạy qua cổng quyền đều ghi log: tool, host,
   quyết định (allow/ask/deny), mode — không ghi nội dung đã gõ hay cookie.
9. **Submit và hành động phá hủy cần duyệt riêng**: `browser_click`/`browser_type`
   không được tự cho phép ở bất kỳ permission mode nào — kể cả `AcceptEdits`,
   `BypassPermissions`, `DontAsk` (xem điểm 3, 4 trên). Riêng hai trường hợp sau
   luôn cần một lượt duyệt riêng của người dùng, không gộp vào lượt duyệt của
   tool call trước đó: (a) submit form (`browser_type` với `submit: true`, hoặc
   click nút submit); (b) hành động phá hủy hoặc không đảo ngược được (xóa, hủy,
   mua, gửi tiền, gửi dữ liệu ra ngoài). Prompt xin quyền phải nêu rõ hành động
   cụ thể (submit form nào, xóa/mua/gửi cái gì, tới đâu) để người dùng duyệt
   đúng việc, không duyệt mù.

## 5. Đăng nhập và submit form (thao tác đổi trạng thái nhạy cảm)

- **Không tự đăng nhập**: agent không tự điền credential vào form đăng nhập khi chưa
  có quyết định rõ ràng của người dùng cho đúng trang đó. `browser_type` vào field
  `type=password` và `browser_click` vào nút submit của form chứa field mật khẩu
  luôn thuộc loại `BrowserLogin`: bắt buộc hỏi (ask) ở mọi mode, kể cả
  `AcceptEdits` và `BypassPermissions` — chỉ người dùng duyệt tay mới chạy.
- Credential không nằm trong tool call: agent không nhận mật khẩu/token qua tham số
  tool. Nếu trang yêu cầu đăng nhập, tool trả lỗi hướng dẫn người dùng đăng nhập
  bằng tay rồi agent tiếp tục từ session đã đăng nhập đó (computer-use chặng sau
  mới xem xét sâu hơn — ngoài phạm vi spec này).
- Không lưu cookie/session đăng nhập ra đĩa ngoài `--user-data-dir` tạm của session;
  thư mục này bị xóa khi session kết thúc. Không export cookie sang tool result.
- Submit form không chứa credential (tìm kiếm, lọc, gửi feedback): vẫn cần duyệt
  riêng theo mục 4 điểm 9 — hỏi ở mọi mode, không allow theo rule. Chỉ khác
  submit có credential ở chỗ không qua gate `BrowserLogin`.

## 6. Chính sách an toàn

1. **Không ghi credential vào log**: không log nội dung `browser_type` khi target là
   field mật khẩu; log của `browser_click` chỉ ghi selector + host, không ghi text
   chứa secret; tool result của `browser_type` chỉ trả độ dài, không echo nội dung.
2. **Không tự đăng nhập** (mục 5). Không lưu token/cookie ra file thường, không gửi
   lên Telegram/Notion.
3. **Chỉ `http(s)`**: như WebFetch, từ chối `file://`, `chrome://`, `data:` làm
   target điều hướng từ tool (spike dùng `about:blank` nội bộ thì được, không phải
   qua tool).
4. **Chặn private network**: tái dùng `check_fetch_target` của WebFetch cho
   `browser_navigate` — chặn `localhost`/IP riêng trừ khi `allow_private_network`
   được bật rõ ràng trong phiên.
5. **Session tách biệt**: mỗi phiên agent dùng một `--user-data-dir` tạm riêng, xóa
   sau khi xong; không dùng chung profile trình duyệt của người dùng.
6. **Giới hạn tài nguyên**: tối đa 1 browser + 1 tab mỗi session ở chặng này; timeout
   mặc định theo mục 2; screenshot cap kích thước viewport hợp lý; `browser_content`
   cap ký tự.
7. **Không chạy JS tùy ý từ agent**: agent không có tool `browser_evaluate` chạy JS
   bất kỳ ở chặng này — chỉ các thao tác có cấu trúc ở mục 2. (Nếu sau này cần,
   đó là quyết định riêng của Leader vì JS tùy ý = thực thi mã trên trang.)
8. **CDP chỉ loopback**: remote-debugging bind `127.0.0.1`, port ngẫu nhiên, không
   expose.
9. **Kiểm soát request mạng toàn phiên**: chỉ kiểm tra URL ban đầu ở
   `browser_navigate` là chưa đủ — redirect và request phụ (script, XHR/fetch,
   ảnh) vẫn có thể đi tới mạng riêng. Bật chặn ở tầng CDP cho toàn bộ phiên ngay
   khi session mở: dùng `Fetch.enable` (pattern `*`, `requestStage: Request`)
   hoặc `Network.setRequestInterception`, chặn (abort) mọi request mà
   `check_fetch_target` của WebFetch từ chối (localhost/IP riêng/metadata
   endpoint), kể cả request phát sinh sau redirect. Request bị chặn thì tool trả
   lỗi rõ ràng, ghi log host + lý do chặn, không thử lại bằng đường khác. Khi
   `navigate` gặp redirect, mỗi URL đích đều kiểm tra lại trước khi theo.
10. **Serialize selector an toàn** (xem mục 3.4): không bao giờ ghép chuỗi
    JavaScript với dữ liệu agent cung cấp.

## 7. Lộ trình implement (để Leader giao sau)

1. Client CDP thô (`browser.rs`): khởi động, bắt tay, gửi lệnh, chờ event, dọn dẹp.
   Kèm unit test với fake CDP server (WebSocket giả trên loopback).
2. 4 tool read-only (`navigate`, `content`, `screenshot`, `wait`) + test trên trang
   JS công khai.
3. 2 tool mutating (`click`, `type`) + nối cổng permissions theo mục 4 + test xin quyền.
4. `BrowserLogin` gate + redaction log (mục 5, 6) + test regression: gõ vào field
   password không lọt vào log.
5. Kiểm soát request mạng toàn phiên (mục 6 điểm 9) + serialize selector an toàn
   (mục 3.4) + test regression: request tới IP riêng bị abort, selector chứa
   quote không phá được expression.
6. Docs tool (`docs/tools/browser-*.md`) theo mẫu `docs/tools/webfetch.md`.

Ngoài phạm vi chặng này: computer-use (điều khiển desktop), lưu session đăng nhập
lâu dài, multi-tab, chạy JS tùy ý.

## 8. Môi trường kiểm chứng + spike

Đo thật trên box `zencode` ngày 27/11/2026:

- Chromium: `155.0.8059.79` (`chromium --version`), gói Debian
  `155.0.8059.79-1~deb13u1` trên `trixie` (`apt-cache policy chromium`);
  candidate mới hơn `156.0.8078.79-1~deb13u1` có sẵn nhưng KHÔNG nâng trong spike.
  Cài trên box: `sudo apt install chromium` (gói `chromium`, không phải snap,
  không phải `chromium-browser`). `/json/version` trả `Protocol-Version: 1.3`,
  `V8-Version: 15.5.35.23`.
- **Chạy lại sau review (27/11/2026 ~21:26 Asia/Saigon)**: cùng Chromium
  `155.0.8059.79` (box `zencode`, chưa nâng), spike chạy lại exit 0, screenshot tái
  tạo đúng **75878 bytes**, SHA256
  `e03d93385e708c609110fb4f9c6380f656b0478a81e7e842467b44feb1f6b33d` (khớp ảnh đã
  commit, chạy lại 2 lần đều giống) — xem `RUN_LOG.md`. Z Tester chạy lại trên máy
  khác ra 77077 byte, SHA256
  `5988988d1a6b2cbf34c4b881f785eef4b7ea77273ff24379e5ff5b4b2ca3708e`, với Chromium
  `154.0.8037.57`: ảnh chụp **phụ thuộc phiên bản Chromium** (render khác nhau
  giữa các bản), nên SHA screenshot chỉ có nghĩa khi đi kèm đúng phiên bản
  Chromium. **Cách xác minh phiên bản** (ghi cả ba vào `RUN_LOG.md` mỗi lần chạy
  lại): `chromium --version`, dòng `CDP: browser=Chrome/<ver>` trong log chạy, và
  `apt-cache policy chromium` (gói Debian đã cài). Nâng Chromium thì chạy lại
  spike và cập nhật SHA mới — không giữ SHA cũ.
- Rust: cargo 1.99.0, rustc 1.99.0.
- Spike: `examples/browser-cdp-spike/` — crate độc lập (có `[workspace]` riêng để
  tách khỏi workspace chính), dùng `tokio-tungstenite 0.26.2` + `serde_json` +
  `reqwest 0.12` + `base64 0.22`. Chạy:
  `cargo run --release` trong `examples/browser-cdp-spike/`.
  Spike khởi động Chromium hệ thống, navigate tới `https://quotes.toscrape.com/js/`
  (trang công khai, nội dung chỉ xuất hiện sau khi JS chạy), đếm `.quote` sau JS,
  đọc `innerText`, chụp screenshot PNG thật. Xem `examples/browser-cdp-spike/README.md`
  và `RUN_LOG.md` để biết kết quả chạy thật (đường dẫn + kích thước screenshot).
- **Kết quả chạy thật (27/11/2026 ~20:40 +07, exit 0)**: build release 29.73s;
  Chromium 155.0.8059.79, CDP protocol 1.3, V8 15.5.35.23; navigate tới
  `https://quotes.toscrape.com/js/`, load event fired, `readyState=complete`,
  jQuery đã load, **10 quote render bởi JS**, `innerText` 1499 ký tự (trích: Einstein,
  J.K. Rowling); screenshot `examples/browser-cdp-spike/screenshot.png`,
  **75878 bytes**, PNG 1280x713, SHA256
  `e03d93385e708c609110fb4f9c6380f656b0478a81e7e842467b44feb1f6b33d`;
  decode PNG kiểm chứng 184 màu khác nhau trong mẫu — ảnh render thật.
  Chi tiết đầy đủ trong `examples/browser-cdp-spike/RUN_LOG.md`.
- Trang spike chỉ là trang công khai, không credential, không đăng nhập.

## 9. Câu hỏi chờ Leader quyết

1. Module browser đặt ở `crates/zen-tools/src/browser.rs` hay crate riêng mới?
2. Có cho `browser_type` gõ vào field password khi người dùng duyệt tay từng lần
   (như mục 5) hay cấm hẳn ở chặng này?
3. Screenshot lưu trong thư mục phiên hay thư mục tạm hệ thống?
4. Có cần `browser_evaluate` (chạy JS tùy ý) ở chặng sau không, hay giữ cấm?
