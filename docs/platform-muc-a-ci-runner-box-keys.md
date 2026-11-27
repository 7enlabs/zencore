# Mục A: khoá CI / runner / box (ZC Platform, 27/11/2026)
Nguồn: study-bao-mat-coding-agent-2026-11-27 (SHA c722bb2a). Trạng thái: DỰ THẢO chờ Leader duyệt, chưa áp thật, chưa đổi khoá nào.

## 1. Kiểm kê (đã đọc từ .github/workflows trên main)
| Nơi | Khoá | Loại | Dài hạn? | Dùng ở |
|---|---|---|---|---|
| CI GitHub Actions | GITHUB_TOKEN | tự cấp mỗi job | Không (hết hạn cuối job) | nhiều workflow; permissions đã khai ở ci, floor-guard, instruction-drift, pr-agent-pilot, release, windows-test |
| CI | OPENROUTER_API_KEY | secret repo | CÓ (đến khi xoay tay) | pr-agent-pilot.yml |
| CI | RENOVATE_TOKEN | secret repo (PAT/App token) | CÓ nếu là PAT | renovate.yml |
| Runner self-hosted `zc-self` | chưa thấy secret trong workflow (runner-smoke chỉ chạy uname) | - | CẦN KIỂM tại máy runner: env, ~/.config, file credential | runner-smoke.yml |
| Box (máy làm việc agent) | cần liệt kê tại box: biến môi trường dạng *_KEY/*_TOKEN, credential helper git, gh auth | - | CHƯA KIỂM; ghế chỉ thấy tên biến, không đọc giá trị | - |

Ghi chú: workflow ở các file khác chưa dùng secret nào ngoài ba mục trên (grep `secrets.*`). Mọi workflow nên có `permissions:` ở cấp file; ci.yml/release.yml/windows-test.yml chỉ khai ở cấp job: cần rà job còn lại.

## 2. Phương án thay theo loại
1. GITHUB_TOKEN: giữ; đặt `permissions: contents: read` mặc định mỗi workflow, nâng quyền theo job. Repo Settings > Actions > Workflow permissions = Read.
2. OPENROUTER_API_KEY: tạo khoá riêng cho CI (không dùng chung khoá người), đặt hạn mức chi tiêu/ngày trên OpenRouter, dùng model free cho pilot, xoay mỗi 30 ngày hoặc ngay khi nghi lộ; chỉ cấp cho workflow pull_request từ nhánh trong repo (không cho fork), đặt trong Environment có bảo vệ.
3. RENOVATE_TOKEN: thay PAT bằng GitHub App cài trên đúng repo (token cài đặt sống 1 giờ, quyền contents/pull_requests write tối thiểu). Nếu chưa dùng App được: fine-grained PAT một repo, hạn 30-90 ngày, nhắc xoay.
4. Khoá nhà cung cấp cloud (nếu có sau này, ví dụ AWS/GCP): OIDC của GitHub (`id-token: write`) đổi lấy khoá tạm, không lưu secret dài hạn.
5. Runner self-hosted: chạy bằng user riêng không sudo, không để khoá trong env của dịch vụ runner; nạp khoá ngắn hạn mỗi job từ bước đầu job; runner chỉ nhận job từ repo/nhánh tin cậy (không PR từ fork); nên ephemeral (`--ephemeral`) nếu khả thi; dọn workspace sau job.
6. Box: khoá để trong kho bí mật (vault) tiêm theo lệnh, không `export` vào shell dài hạn; không ghi khoá vào file trong repo (đã có gitleaks/trufflehog trong secrets.yml); một khoá một mục đích, hẹp quyền; đặt PYTHONDONTWRITEBYTECODE=1.
Theo Wiz: khoá lộ bị dùng sau ~30 phút; giới hạn phạm vi (ví dụ giới hạn model/vùng) làm kẻ tấn công mất thời gian; hạn mức chi tiêu là lớp chặn cuối.

## 3. Checklist thực thi (mỗi dòng cần Leader duyệt trước khi áp)
- [ ] Liệt kê tên (không giá trị) mọi secret repo/org: `gh secret list` và ngày cập nhật. Chủ: ZC Platform (quyền đọc tên).
- [ ] Rà `permissions:` từng job trong 10 workflow. L1, làm được ngay trên nhánh.
- [ ] Tạo OpenRouter khoá CI mới + hạn mức/ngày, xoay khoá cũ. Cần: chủ tài khoản OpenRouter (người dùng/Zen). L3.
- [ ] Chuyển Renovate sang GitHub App hoặc PAT một repo có hạn. Cần: admin org. L3.
- [ ] Kiểm runner `zc-self`: user chạy, env dịch vụ, file credential, ephemeral. Cần quyền SSH box. L3.
- [ ] Kiểm box: danh sách biến *_KEY/*_TOKEN và credential helper; chuyển sang tiêm theo lệnh. L3.
- [ ] Bật alert chi phí (mục 4). Cần: chủ tài khoản nhà cung cấp.

## 4. Ngưỡng cảnh báo chi phí và cách phát hiện
Chưa có số liệu chi tiêu thật trong tay ghế nên ngưỡng dưới đây là ĐỀ XUẤT, cần Leader/Zen chốt theo ngân sách:
- Hạn mức cứng/ngày trên mỗi khoá CI (OpenRouter): đặt thấp (ví dụ vài USD), khoá tự ngừng khi chạm.
- Cảnh báo mềm: chi tiêu ngày > 3 lần trung bình 7 ngày, hoặc số lời gọi/giờ > 5 lần trung bình, hoặc có lời gọi ngoài giờ làm việc/model lạ.
- Phát hiện: xuất log dùng theo khoá (dashboard nhà cung cấp) mỗi ngày vào Team Metrics (Grafana, việc kế tiếp); workflow pr-agent chỉ chạy khi PR mở, nên lời gọi ngoài sự kiện PR là dấu hiệu bất thường.
- Khi nghi lộ: thu hồi khoá ngay, xoay, xem log 24 giờ trước, báo Leader.

## 5. Giới hạn
Chưa kiểm được box và runner (không có quyền/ chưa truy cập); ngưỡng là đề xuất; không đổi khoá nào trong tài liệu này.
