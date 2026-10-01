# Ferry's desktop app, in Simplified Chinese.
#
# Written by an AI agent from i18n/en-US/ferry.ftl and not yet reviewed by
# a native speaker: corrections are welcome as pull requests. The comments
# describing each message are in en-US/ferry.ftl; keys and arguments must
# match that file (`ui::i18n::tests` checks). Chinese has one plural form,
# so counts need no selector. See docs/archive/PLAN_I18N.md.

## Files dropped on the window or the tray icon (src/ui/drops.rs)

drop-send-to-header = 将 { $count } 个文件发送到：
drop-no-recipients = 没有可以接收文件的已配对设备
drop-send-failed = 无法发送
drop-folders-refused = 只能发送文件，不能发送文件夹。

## Files dropped on the window: the hint and the chooser (src/ui/overlay/drop.rs)

drop-choose-hint = 拖放到任意位置以选择设备
drop-chooser-title-one = 发送文件
drop-chooser-title = 发送 { $count } 个文件
drop-chooser-prompt = 选择要发送到的设备：
drop-chooser-no-devices = 没有已连接且可以接收文件的已配对设备。

## Dialogs (src/ui/overlay/dialog.rs; also the drop chooser)

dialog-cancel = 取消
dialog-counter = { $count }/{ $max }

## An incoming pairing request (src/ui/overlay/incoming.rs)

incoming-title = 配对请求
incoming-body = { $name } 请求与此电脑配对。请仅在对方显示相同代码时接受：
incoming-queued = 还有 { $count } 个请求在等待
incoming-reject = 拒绝
incoming-accept = 接受

## Errors, keyed by the code the HTTP API reports (src/ui/error.rs)

error-daemon_unavailable = Ferry 没有响应。
error-device_not_found = 该设备已不在设备列表中。
error-device_not_connected = 该设备当前未连接。
error-already_paired = 该设备已配对。
error-pairing_in_progress = 已在与该设备配对。
error-pairing_not_found = 该配对请求已不存在。
error-invalid_pairing_state = 该配对请求已失效。
error-device_not_paired = 该设备未配对。
error-unsupported_by_peer = 该设备不支持此操作。
error-invalid_file_name = 无法发送使用该名称的文件。
error-transfer_too_large = 文件太大，无法发送。
error-transfer_not_found = 该传输已不存在。
error-invalid_transfer_state = 该传输已经结束。
error-invalid_device_name = 请使用 1 到 32 个字符，不含 . , : ; ! ? ( ) [ ] < > 或引号。
error-invalid_download_dir = 该文件夹不能用于保存下载。
error-invalid_address = 请输入 IPv4 地址（如 192.168.1.20）或主机名（如 phone.example.net）。
error-unresolvable_address = 无法将该名称解析为 IPv4 地址。请检查拼写以及 DNS 或 VPN。
error-address_unreachable = 该地址没有设备应答。请确认那里正在运行 Ferry 或 KDE Connect。
error-too_many_addresses = 每台设备最多保存 8 个地址。
error-unknown = 出了点问题（{ $code }）。
error-send-file-failed = 无法发送 { $file }：{ $reason }
error-send-files-failed = 无法发送 { $count } 个文件：{ $reason }
error-upload-file-failed = 无法上传 { $file }：{ $reason }
error-upload-files-failed = 无法上传 { $count } 个文件：{ $reason }

## Starting the app (src/ui/launch.rs, src/ui/pages/startup.rs)

app-window-title = Ferry
startup-starting = 正在启动 Ferry…
startup-failed = Ferry 无法启动。
    { $error }

## The shell: the window's own messages and dialogs (src/ui/mod.rs,
## src/ui/shell.rs, src/ui/actions.rs)

shell-notification-toast = { $title }：{ $body }
shell-unknown-device = 该设备已不在设备列表中。
shell-unpair-title = 取消配对？
shell-unpair-body = 取消后，{ $name } 需要重新配对才能与此电脑交换内容。
shell-unpair-confirm = 取消配对
shell-open-failed = 无法打开 { $path }
shell-open-link-failed = 无法打开 { $url }
shell-start-on-login-failed = 无法更改登录时启动的设置。
shell-add-by-address-title = 通过地址添加
shell-add-by-address-label = IP 地址或主机名
shell-add-by-address-helper = 该设备上必须正在运行 Ferry 或 KDE Connect。Ferry 会连接并配对，然后记住该地址。
shell-add-by-address-confirm = 添加
shell-address-add-title = 添加地址
shell-address-edit-title = 编辑地址
shell-address-body = 当在网络中找不到 { $name } 时，Ferry 会尝试此地址。
shell-address-save = 保存
shell-rename-title = 设备名称
shell-rename-helper = 此电脑在你的其他设备上显示的名称
shell-rename-confirm = 保存
shell-download-dir-title = 接收的文件保存到
shell-autostart-comment = 在系统托盘中启动 Ferry

## The tray menu (src/ui/background.rs)

tray-open = 打开 Ferry
tray-no-paired-devices = 没有已配对的设备
tray-no-devices-connected = 没有已连接的设备
tray-show-details = 显示详情
tray-settings = 设置
tray-about = 关于 Ferry
tray-quit = 退出

## Desktop notifications (src/ui/background.rs, src/ui/desktop/notify.rs)

notify-pairing-title = 配对请求
notify-pairing-body = { $name } 请求与此电脑配对。
notify-file-received-title = 已收到文件
notify-file-received-body = { $file }，来自 { $name }
notify-open = 打开

## Widgets the pages share (src/ui/widgets.rs)

widget-back = 返回
widget-retry = 重试
widget-size-bytes = { $count } 字节
widget-size-kb = { $size } KB
widget-size-mb = { $size } MB
widget-size-gb = { $size } GB
widget-size-tb = { $size } TB

## The device list, the home page (src/ui/pages/devices.rs)

devices-title = 设备
devices-settings = 设置
devices-transfers = 传输
devices-this-computer = 此电脑：{ $name }
devices-add = 添加设备
devices-loading = 正在加载设备…
devices-empty = 还没有已配对的设备
devices-find = 查找要配对的设备
device-reachability-connected = 已连接
device-reachability-nearby = 在附近
device-reachability-unavailable = 无法访问

## One device's page (src/ui/pages/device.rs)

device-title = 设备
device-unknown = 该设备已不在设备列表中。
device-recent-transfers = 最近的传输
device-see-all = 查看全部
device-id = 设备 ID
device-type = 类型
device-protocol-version = 协议版本
device-type-desktop = 台式机
device-type-laptop = 笔记本电脑
device-type-phone = 手机
device-type-tablet = 平板电脑
device-type-tv = 电视
device-unpair = 取消配对
device-addresses-title = 地址
device-addresses-detail = 当在网络中找不到该设备时，Ferry 会尝试这些地址。
device-addresses-none = 没有已保存的地址
device-address-add = 添加地址
device-address-edit = 编辑
device-address-remove = 移除

## Adding a device (src/ui/pages/add_device.rs)

add-device-title = 添加设备
add-device-scan = 重新扫描
add-device-instructions = 在另一台设备上打开 Ferry 或 KDE Connect，并确保两台设备在同一网络中。
add-device-none-found = 未找到设备
add-device-pairing = 正在配对
add-device-not-connected = 未连接
add-device-pair = 配对
add-device-by-address = 通过 IP 地址或主机名添加
add-device-by-address-detail = 适用于设备不会自动出现的网络
add-device-connecting = 正在连接 { $address }…
add-device-connecting-detail = 正在等待设备应答
add-device-cancel = 取消

## Pairing with a device (src/ui/pages/pairing.rs)

pairing-title = 配对
pairing-unknown = 该配对请求已不存在。
pairing-waiting = 正在等待 { $name }
pairing-waiting-detail = 请确认 { $name } 显示相同的代码，然后在该设备上接受请求。
pairing-accepted = 已与 { $name } 配对
pairing-rejected = 配对被拒绝
pairing-rejected-detail = 请求被拒绝或已取消。
pairing-expired = 请求超时
pairing-expired-detail = { $name } 未及时应答。
pairing-failed = 配对失败
pairing-failed-detail = 与 { $name } 的连接已断开。
pairing-cancel = 取消
pairing-done = 完成
pairing-close = 关闭
pairing-try-again = 重试

## File transfers (src/ui/pages/transfers.rs)

transfers-title = 传输
transfers-loading = 正在加载传输…
transfers-empty = 还没有传输
transfers-from = 来自 { $name } · { $status }
transfers-to = 发往 { $name } · { $status }
transfers-cancel = 取消
transfers-open-file = 打开文件
transfers-open-folder = 打开文件夹
transfers-queued = 等待中
transfers-connecting = 正在连接
transfers-progress = { $done } / { $total }
transfers-cancelled = 已取消
transfers-failed = 失败
transfers-failed-connection_failed = 失败：连接中断
transfers-failed-timed_out = 失败：超时
transfers-failed-unavailable = 失败：被接收方拒绝
transfers-failed-protocol_error = 失败：设备发送了意外的内容

## Settings (src/ui/pages/settings.rs)

settings-title = 设置
settings-loading = 正在加载设置…
settings-device-name = 设备名称
settings-download-dir = 接收的文件保存到
settings-close-to-tray = 关闭窗口后继续运行
settings-close-to-tray-detail = 保留在系统托盘中，以便设备仍能访问此电脑
settings-start-on-login = 登录时启动
settings-start-on-login-detail = 在系统托盘中打开，随时为你的设备待命
settings-language = 语言
settings-language-detail = 用于应用的窗口、托盘菜单和通知
settings-language-system = 跟随系统
settings-language-own-name = 简体中文
settings-appearance = 外观
settings-appearance-detail = 浅色或深色，或与系统一致
settings-appearance-system = 跟随系统
settings-appearance-light = 浅色
settings-appearance-dark = 深色
settings-cli = 命令行访问
settings-cli-detail = 允许 ferry-cli 控制此应用
settings-cli-setup-hint = 你在此电脑上运行的 ferry-cli 会自动通过以下位置找到本应用：
settings-cli-installed-at = ferry-cli 的安装位置
settings-cli-not-listening = ferry-cli 无法访问本应用：{ $error }
settings-cli-change-failed = 无法更改命令行访问：{ $error }
settings-about = 关于 Ferry
settings-version = 版本 { $version }

## About (src/ui/pages/about.rs); the app's name, Ferry, isn't translated

about-title = 关于
about-version = 版本 { $version }
about-description = 适用于 macOS、Linux 和 Windows 的 KDE Connect 客户端。
about-author = 由 Fanchao 开发
about-source = 源代码
about-support = 支持开发
about-support-detail = 在 GitHub 上赞助
about-licenses = 开源许可证
about-licenses-detail = Ferry 所依赖的软件

## Ping (src/ui/features/ping.rs)

ping-action = Ping
ping-received = Ping！
ping-sent = 已 Ping { $name }。
ping-failed = 无法 Ping { $name }

## Find my phone (src/ui/features/findmyphone.rs)

findmyphone-action = 响铃
findmyphone-sent = 已让 { $name } 响铃。
findmyphone-failed = 无法让 { $name } 响铃

## Battery (src/ui/features/battery.rs)

battery-charge = { $charge }%

## Connectivity (src/ui/features/connectivity.rs)

connectivity-unknown-network = 移动网络

## Sending files (src/ui/features/share.rs)

share-action = 发送文件
share-drop-hint = 拖放以发送到 { $name }
share-pick-title = 发送文件到 { $name }
share-failed = 无法发送到 { $name }
share-error-file-unreadable = 无法读取该文件。
share-text-action = 发送文本
share-text-title = 发送文本或链接
share-text-to = 发送到 { $name }
share-text-label = 文本或链接
share-text-send = 发送
share-error-share_empty = 请输入要发送的文本或链接。
share-error-share_too_large = 文本太长，无法发送。
share-text-sent = 已将文本发送到 { $name }。
share-link-sent = 已将链接发送到 { $name }。
share-text-copied = 共享的文本已复制到剪贴板。
share-text-not-copied = 共享的文本太长，无法复制到剪贴板。
share-link-opened = 已在浏览器中打开共享的链接。
share-link-failed = 无法在浏览器中打开共享的链接。

## The clipboard (src/ui/features/clipboard.rs)

clipboard-action = 发送剪贴板
clipboard-sync = 同步剪贴板
clipboard-sync-detail = 与已配对的设备共享复制的文本
clipboard-sent = 已将剪贴板发送到 { $name }。
clipboard-failed = 无法将剪贴板发送到 { $name }
clipboard-error-clipboard_empty = 剪贴板上没有可发送的文本。
clipboard-error-clipboard_text_too_large = 剪贴板中的文本太长，无法发送。

## A device's notifications (src/ui/features/notifications.rs)

notifications-action =
    { $count ->
        [0] 通知
       *[other] 通知（{ $count }）
    }
notifications-title = 通知
notifications-offline = { $name } 未连接。
notifications-offline-detail = 连接后，它的通知会显示在这里。
notifications-empty = 没有来自 { $name } 的通知。
notifications-empty-detail = 请在手机上允许 KDE Connect 读取通知，并选择要共享通知的应用。
notifications-sync = 显示通知
notifications-sync-detail = 在此列出并提醒 { $name } 共享的通知。
notifications-sync-failed = 无法更改此设置
notifications-reply = 回复
notifications-dismiss = 忽略
notifications-reply-title = 回复
notifications-reply-to = 回复“{ $title }”
notifications-reply-label = 消息
notifications-reply-send = 发送
notifications-reply-sent = 已发送回复。
notifications-reply-failed = 无法发送回复
notifications-action-failed = 无法执行“{ $action }”
notifications-dismiss-failed = 无法忽略该通知
notifications-announce-title = { $title } · { $name }
notifications-error-notification_not_found = 该通知已不在设备上。
notifications-error-notification_not_repliable = 该通知不支持回复。
notifications-error-notification_not_dismissable = 无法在此处忽略该通知。
notifications-error-unknown_notification_action = 该通知已没有这个按钮。
notifications-error-empty_reply = 请输入消息。

## A phone's calls (src/ui/features/telephony.rs)

telephony-incoming-call = { $caller } 来电
telephony-missed-call = { $caller } 的未接来电
telephony-unknown-caller = 未知来电者
telephony-mute = 静音
telephony-mute-action = 铃声静音
telephony-muted = 已让 { $name } 将铃声静音。
telephony-mute-failed = 无法将 { $name } 静音
telephony-error-not_ringing = 现在没有来电在响铃。
telephony-status-ringing = 来电中
telephony-status-talking = 通话中

## Browsing a device's files (src/ui/features/browse/)

browse-action = 浏览文件
browse-title = { $name } 上的文件
browse-not-shared = { $name } 未共享其文件。
browse-not-connected = 连接 { $name } 以浏览其文件。
browse-upload = 上传文件
browse-new-folder = 新建文件夹
browse-refresh = 刷新
browse-hide-hidden = 不显示隐藏文件
browse-show-hidden = 显示隐藏文件
browse-up = 上一级
browse-loading-folder = 正在加载文件夹…
browse-connecting = 正在连接设备…
browse-no-storage = 该设备未共享任何存储空间。
browse-empty-folder = 此文件夹为空。将文件拖放到这里即可上传。
browse-storage = 存储空间
browse-column-name = 名称
browse-column-size = 大小
browse-column-modified = 修改时间
browse-more = 更多
browse-preview = 预览
browse-download = 下载
browse-rename = 重命名
browse-delete = 删除
browse-downloading = 正在下载 { $file }
browse-see-transfers = 传输
browse-drop-hint = 拖放以上传到 { $folder }
browse-upload-title = 上传文件到 { $folder }
browse-name-label = 名称
browse-new-folder-title = 新建文件夹
browse-new-folder-confirm = 创建
browse-rename-title = 重命名
browse-rename-confirm = 重命名
browse-name-empty = 请输入名称。
browse-name-reserved = 该名称为保留名称。
browse-name-slash = 名称不能包含“/”。
browse-delete-folder-title = 删除文件夹？
browse-delete-folder-body = “{ $name }”及其中的所有内容将从设备上删除。此操作无法撤销。
browse-delete-file-title = 删除文件？
browse-delete-file-body = “{ $name }”将从设备上删除。此操作无法撤销。
browse-delete-confirm = 删除
browse-loading-image = 正在加载图片…
browse-close-preview = 关闭
browse-image-unreadable = 无法显示此图片。
browse-error-file-unreadable = 无法读取该文件。
browse-error-files_unavailable = 该设备未共享其文件。请在设备上的 KDE Connect 中，在“文件系统共享”插件里允许访问文件。
browse-error-files_unavailable-reason = 该设备未共享其文件（{ $reason }）。请在设备上的 KDE Connect 中，在“文件系统共享”插件里允许访问文件。
browse-error-file_not_found = 该文件或文件夹已不存在。
browse-error-file_exists = 已存在同名的文件或文件夹。
browse-error-file_permission_denied = 该设备不允许此操作。
browse-error-not_a_directory = 那不是文件夹。
browse-error-is_a_directory = 只能下载文件，不能下载文件夹。
browse-error-invalid_path = 无法使用该名称或位置。
browse-error-files_failed = 无法访问该设备的文件。
browse-error-files_timed_out = 设备响应超时。
browse-error-files_host_key_mismatch = 该设备的文件服务器无法证明它就是已配对的设备，因此 Ferry 没有连接。

## Outside the app: the menu entry, the installer and the system's own
## texts about it (packaging/, through packaging/i18n.sh)
##
## Plain text on one line: no { }, as the packaging scripts copy it as it
## is.

package-generic-name = 设备连接
package-comment = 通过局域网与你的设备配对，并共享文件和剪贴板
package-keywords = KDE Connect;手机;配对;剪贴板;文件传输;phone;pair;clipboard;file transfer;
package-local-network-usage = Ferry 会在局域网中查找并连接你的设备。
package-start-app = 启动 Ferry
