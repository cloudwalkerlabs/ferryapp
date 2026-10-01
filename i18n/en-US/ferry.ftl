# Ferry's desktop app, in US English: the source of every other language,
# and the fallback for any message a translation lacks. `fl!` checks keys
# and arguments against this file at compile time.
#
# Keys are prefixed by feature (`browse-…`, `clipboard-…`, `error-<code>`),
# grouped under a header per feature. Whole sentences only; names, paths
# and numbers are arguments; every count goes through a plural selector.
# See docs/archive/PLAN_I18N.md.

## Files dropped on the window or the tray icon (src/ui/drops.rs)

# The header of the tray menu that asks where to send files dropped on the
# tray icon; the device names follow as menu items.
drop-send-to-header =
    { $count ->
        [one] Send { $count } file to:
       *[other] Send { $count } files to:
    }
drop-no-recipients = No paired device can receive files
# The title of a notification (or the start of a toast) when dropped files
# can't be sent; the reason is its body.
drop-send-failed = Couldn’t send
drop-folders-refused = Only files can be sent, not folders.

## Files dropped on the window: the hint and the chooser (src/ui/overlay/drop.rs)

# The hint while files hover over a page with no device to drop them on.
drop-choose-hint = Drop anywhere to choose a device
# The chooser's title for one file; the file's name goes under it.
drop-chooser-title-one = Send file
# The chooser's title for several files (never 1 in English, but other
# languages' `one` may cover counts like 21).
drop-chooser-title =
    { $count ->
        [one] Send { $count } file
       *[other] Send { $count } files
    }
drop-chooser-prompt = Choose the device to send to:
drop-chooser-no-devices = No paired device is connected and able to receive files.

## Dialogs (src/ui/overlay/dialog.rs; also the drop chooser)

dialog-cancel = Cancel
# Under a text field with a limit: characters typed, and how many fit.
dialog-counter = { $count }/{ $max }

## An incoming pairing request (src/ui/overlay/incoming.rs)

incoming-title = Pairing request
# `name` is the device asking.
incoming-body = { $name } wants to pair with this computer. Accept only if it shows the same code:
# Other requests waiting behind this one.
incoming-queued =
    { $count ->
        [one] { $count } more request waiting
       *[other] { $count } more requests waiting
    }
incoming-reject = Reject
incoming-accept = Accept

## Errors, keyed by the code the HTTP API reports (src/ui/error.rs)

error-daemon_unavailable = Ferry is not responding.
error-unauthorized = Ferry rejected this app’s access token.
error-device_not_found = That device is no longer known.
error-device_not_connected = The device is not connected right now.
error-already_paired = The device is already paired.
error-pairing_in_progress = A pairing with this device is already running.
error-pairing_not_found = That pairing request no longer exists.
# Also for `invalid_pairing_direction`.
error-invalid_pairing_state = That pairing request is no longer active.
error-device_not_paired = The device is not paired.
error-unsupported_by_peer = The device doesn’t support that.
error-invalid_file_name = That file name can’t be sent.
# Also for `payload_too_large`.
error-transfer_too_large = The file is too large to send.
error-transfer_not_found = That transfer no longer exists.
error-invalid_transfer_state = That transfer has already finished.
error-request_timeout = Ferry took too long to respond.
error-invalid_device_name = Use 1 to 32 characters, without . , : ; ! ? ( ) [ ] < > or quotes.
error-invalid_download_dir = That folder can’t be used for downloads.
error-invalid_address = Enter an IPv4 address like 192.168.1.20, or a hostname like phone.example.net.
error-unresolvable_address = That name couldn’t be resolved to an IPv4 address. Check the spelling and your DNS or VPN.
error-address_unreachable = No device answered at that address. Make sure Ferry or KDE Connect is running there.
error-too_many_addresses = A device can keep up to 8 addresses.
# Any other code; `code` is the API's, such as `internal_error`.
error-unknown = Something went wrong ({ $code }).
# One of the files being sent failed; `reason` is a sentence saying why.
error-send-file-failed = Couldn’t send { $file }: { $reason }
# Several files being sent failed; `reason` is why the first one did.
error-send-files-failed =
    { $count ->
        [one] Couldn’t send { $count } file: { $reason }
       *[other] Couldn’t send { $count } files: { $reason }
    }
# The same, for files being uploaded to a device's folder.
error-upload-file-failed = Couldn’t upload { $file }: { $reason }
error-upload-files-failed =
    { $count ->
        [one] Couldn’t upload { $count } file: { $reason }
       *[other] Couldn’t upload { $count } files: { $reason }
    }

## Starting the app (src/ui/launch.rs, src/ui/pages/startup.rs)

# The window's title: the app's name.
app-window-title = Ferry
startup-starting = Starting Ferry…
# `error` is why, in English (it comes from the logs' wording).
startup-failed = Ferry could not start.
    { $error }

## The shell: the window's own messages and dialogs (src/ui/mod.rs,
## src/ui/shell.rs, src/ui/actions.rs)

# A notification shown as a toast while the window is focused.
shell-notification-toast = { $title }: { $body }
shell-unknown-device = This device is no longer known.
shell-unpair-title = Unpair device?
shell-unpair-body = { $name } will need to be paired again before it can exchange anything with this computer.
shell-unpair-confirm = Unpair
# `path` is the file's full path.
shell-open-failed = Couldn’t open { $path }
# `url` is a web address.
shell-open-link-failed = Couldn’t open { $url }
shell-start-on-login-failed = Couldn’t change starting on login.
shell-add-by-address-title = Add by address
shell-add-by-address-label = IP address or hostname
shell-add-by-address-helper = Ferry or KDE Connect must be running on that device. Ferry connects to it, pairs, and keeps the address.
shell-add-by-address-confirm = Add
# Dialog titles for adding and editing a device's saved address.
shell-address-add-title = Add address
shell-address-edit-title = Edit address
# `name` is the device's name.
shell-address-body = Ferry tries this address when { $name } isn’t found on the network.
shell-address-save = Save
shell-rename-title = Device name
shell-rename-helper = How this computer appears on your other devices
shell-rename-confirm = Save
# The title of the folder picker for the download folder.
shell-download-dir-title = Save received files in
# The comment of the entry that starts the app when the user logs in
# (Linux; shown in the desktop's list of startup applications).
shell-autostart-comment = Start Ferry in the tray

## The tray menu (src/ui/background.rs)

tray-open = Open Ferry
# In place of the devices, when none is paired.
tray-no-paired-devices = No paired devices
# In place of the devices, when none of the paired ones is connected.
tray-no-devices-connected = No devices connected
# The last item of a device's submenu: opens its page.
tray-show-details = Show details
tray-settings = Settings
tray-about = About Ferry
tray-quit = Quit

## Desktop notifications (src/ui/background.rs, src/ui/desktop/notify.rs)

notify-pairing-title = Pairing request
# `name` is the device asking.
notify-pairing-body = { $name } wants to pair with this computer.
notify-file-received-title = File received
# `file` is the file's name, `name` the device's.
notify-file-received-body = { $file } from { $name }
# The button on a notification (on Linux) that opens the window.
notify-open = Open

## Widgets the pages share (src/ui/widgets.rs)

widget-back = Back
widget-retry = Retry

# File sizes (1 KB is 1024 bytes). The number is already written in the
# language's digits and decimal separator, like "1.5" or "1,5".
widget-size-bytes =
    { $count ->
        [one] { $count } byte
       *[other] { $count } bytes
    }
widget-size-kb = { $size } KB
widget-size-mb = { $size } MB
widget-size-gb = { $size } GB
widget-size-tb = { $size } TB

## The device list, the home page (src/ui/pages/devices.rs)

devices-title = Devices
# Tooltips of the header's icon buttons.
devices-settings = Settings
devices-transfers = Transfers
# `name` is this computer's device name.
devices-this-computer = This computer: { $name }
devices-add = Add device
devices-loading = Loading devices…
devices-empty = No paired devices yet
devices-find = Find a device to pair
# How reachable a device is, next to a coloured dot (also on Add device).
device-reachability-connected = Connected
device-reachability-nearby = Nearby
device-reachability-unavailable = Not reachable

## One device's page (src/ui/pages/device.rs)

# The title when the device is no longer known.
device-title = Device
device-unknown = This device is no longer known.
device-recent-transfers = Recent transfers
# A link to the Transfers page.
device-see-all = See all
# Labels above the device's facts.
device-id = Device ID
device-type = Type
device-protocol-version = Protocol version
# The kind of device, shown under "Type"; lower case in English.
device-type-desktop = desktop
device-type-laptop = laptop
device-type-phone = phone
device-type-tablet = tablet
device-type-tv = tv
device-unpair = Unpair
# The device page's list of saved addresses, and its explanation.
device-addresses-title = Addresses
device-addresses-detail = Ferry tries these when the device isn’t found on the network.
device-addresses-none = No addresses saved
device-address-add = Add address
# Tooltips of a saved address's buttons.
device-address-edit = Edit
device-address-remove = Remove

## Adding a device (src/ui/pages/add_device.rs)

add-device-title = Add device
# Tooltip of the header's button that searches the network again.
add-device-scan = Scan again
add-device-instructions = Open Ferry or KDE Connect on the other device and make sure both are on the same network.
add-device-none-found = No devices found
# Why a device can't be paired now, under its name.
add-device-pairing = Pairing in progress
add-device-not-connected = Not connected
add-device-pair = Pair
add-device-by-address = Add by IP address or hostname
add-device-by-address-detail = For networks where the device doesn’t show up on its own
# Shown while an address is tried; `address` is the IPv4 address or hostname. Then a pairing starts.
add-device-connecting = Connecting to { $address }…
add-device-connecting-detail = Waiting for the device to answer
# Stops trying the address.
add-device-cancel = Cancel

## Pairing with a device (src/ui/pages/pairing.rs)

pairing-title = Pairing
pairing-unknown = This pairing request no longer exists.
# `name` is the other device's name.
pairing-waiting = Waiting for { $name }
pairing-waiting-detail = Check that { $name } shows the same code, then accept the request there.
pairing-accepted = Paired with { $name }
pairing-rejected = Pairing declined
pairing-rejected-detail = The request was declined or cancelled.
pairing-expired = Request timed out
pairing-expired-detail = { $name } did not answer in time.
pairing-failed = Pairing failed
pairing-failed-detail = The connection to { $name } was lost.
pairing-cancel = Cancel
pairing-done = Done
pairing-close = Close
pairing-try-again = Try again

## File transfers (src/ui/pages/transfers.rs)

transfers-title = Transfers
transfers-loading = Loading transfers…
transfers-empty = No transfers yet
# A transfer's device and state, in a list that mixes devices. `name` is
# the other device's name, `status` one of the states below (or a size).
transfers-from = From { $name } · { $status }
transfers-to = To { $name } · { $status }
# Tooltips of a transfer's buttons.
transfers-cancel = Cancel
transfers-open-file = Open file
transfers-open-folder = Open folder
# A transfer's state. `done` and `total` are sizes, like "3.0 MB".
transfers-queued = Waiting
transfers-connecting = Connecting
transfers-progress = { $done } of { $total }
transfers-cancelled = Cancelled
transfers-failed = Failed
# Failed, keyed by the core's error code.
transfers-failed-connection_failed = Failed: connection lost
transfers-failed-timed_out = Failed: timed out
transfers-failed-unavailable = Failed: refused by the receiver
transfers-failed-protocol_error = Failed: the device sent something unexpected

## Settings (src/ui/pages/settings.rs)

settings-title = Settings
settings-loading = Loading settings…
settings-device-name = Device name
settings-download-dir = Save received files in
settings-close-to-tray = Keep running when the window is closed
settings-close-to-tray-detail = Stay in the tray so devices can still reach this computer
settings-start-on-login = Start when you log in
settings-start-on-login-detail = Open in the tray, ready for your devices
# The language setting: its title, the line under it, and the first choice
# of its list, which follows the system's language.
settings-language = Language
settings-language-detail = For the app’s window, tray menu and notifications
settings-language-system = System default
# This file's language, named in itself, as Settings' list of languages
# shows it to someone looking for their own: "Deutsch", "简体中文".
settings-language-own-name = English
# The appearance setting: its title, the line under it, and its list's
# choices: follow the system's light or dark mode, always light, always dark.
settings-appearance = Appearance
settings-appearance-detail = Light or dark, or the same as your system
settings-appearance-system = System default
settings-appearance-light = Light
settings-appearance-dark = Dark
settings-cli = Command line access
settings-cli-detail = Let ferry-cli control this app
# Under it, the path to the socket, which only this user can open.
settings-cli-setup-hint = ferry-cli run by you on this computer finds the app by itself, through
# Under it, the path to the `ferry-cli` program.
settings-cli-installed-at = ferry-cli is installed at
# `error` is the reason, in English, like "another Ferry is already serving /home/me/.config/ferry/ferry.sock".
settings-cli-not-listening = ferry-cli can’t reach the app: { $error }
# `error` is the reason, in English.
settings-cli-change-failed = Couldn’t change command line access: { $error }
settings-about = About Ferry
# `version` is the app's, like "1.2.0".
settings-version = Version { $version }

## About (src/ui/pages/about.rs); the app's name, Ferry, isn't translated

about-title = About
# `version` is the app's, like "1.2.0".
about-version = Version { $version }
about-description = A KDE Connect client for macOS, Linux and Windows.
about-author = Made by Fanchao
about-source = Source code
about-support = Support development
about-support-detail = Sponsor on GitHub
about-licenses = Open source licenses
about-licenses-detail = The software Ferry is built on

## Ping (src/ui/features/ping.rs)

# A device's action (a button, and an item in the tray).
ping-action = Ping
# A notification's body when a device pings without a message; the device's
# name is its title.
ping-received = Ping!
# `name` is the device's name.
ping-sent = Pinged { $name }.
ping-failed = Couldn’t ping { $name }

## Find my phone (src/ui/features/findmyphone.rs)

findmyphone-action = Ring
findmyphone-sent = Asked { $name } to ring.
findmyphone-failed = Couldn’t ring { $name }

## Battery (src/ui/features/battery.rs)

# A device's charge, in its status chip; `charge` is 0 to 100, written in
# the language's digits. The percent sign and any space before it (as in
# German's "82 %") belong here.
battery-charge = { $charge }%

## Connectivity (src/ui/features/connectivity.rs)

# A SIM's status chip when the phone doesn't say which mobile network it
# is on (otherwise the chip shows the network's name, like "LTE"); its
# icon shows the signal's bars.
connectivity-unknown-network = Mobile

## Sending files (src/ui/features/share.rs)

share-action = Send files
# The hint while files hover over a device's page; `name` is the device's.
share-drop-hint = Drop to send to { $name }
# The file picker's title.
share-pick-title = Send files to { $name }
# The title of a failure; why is its body.
share-failed = Couldn’t send to { $name }
share-error-file-unreadable = The file couldn’t be read.
# A device's action (a button, and an item in the tray) that asks for text
# or a link to send; a web link is sent as a link, which the device opens.
share-text-action = Send text
# The dialog asking for it: its title, the line under the title (`name`
# is the device's), the field's label and the confirm button.
share-text-title = Send text or a link
share-text-to = To { $name }
share-text-label = Text or link
share-text-send = Send
# Under the field when it is blank, and why sending failed.
share-error-share_empty = Type the text or link to send.
share-error-share_too_large = The text is too long to send.
# `name` is the device's name.
share-text-sent = Sent the text to { $name }.
share-link-sent = Sent the link to { $name }.
# Notification bodies when a device shares text or a link with this
# computer; the device's name is the title.
share-text-copied = Shared text copied to the clipboard.
share-text-not-copied = Shared text was too long to copy to the clipboard.
share-link-opened = Opened a shared link in your browser.
share-link-failed = Couldn’t open a shared link in your browser.

## The clipboard (src/ui/features/clipboard.rs)

clipboard-action = Send clipboard
# The setting's switch and its explanation.
clipboard-sync = Sync clipboard
clipboard-sync-detail = Share copied text with paired devices
clipboard-sent = Sent the clipboard to { $name }.
clipboard-failed = Couldn’t send the clipboard to { $name }
clipboard-error-clipboard_empty = There is no text on the clipboard to send.
clipboard-error-clipboard_text_too_large = The clipboard text is too long to send.

## A device's notifications (src/ui/features/notifications.rs)

# The device's action, with how many notifications it shows.
notifications-action =
    { $count ->
        [0] Notifications
       *[other] Notifications ({ $count })
    }
notifications-title = Notifications
# `name` is the device's.
notifications-offline = { $name } isn’t connected.
notifications-offline-detail = Its notifications show here while it is.
notifications-empty = No notifications from { $name }.
notifications-empty-detail = On the phone, let KDE Connect read notifications, and choose which apps share them.
# The switch on a device's page turning its notifications here on or off.
# `name` is the device's.
notifications-sync = Show notifications
notifications-sync-detail = List and announce the notifications { $name } shares.
notifications-sync-failed = Couldn’t change the setting
# A notification's buttons.
notifications-reply = Reply
notifications-dismiss = Dismiss
# The dialog asking for a reply. `title` is the notification's title (or its
# app's name).
notifications-reply-title = Reply
notifications-reply-to = To “{ $title }”
notifications-reply-label = Message
notifications-reply-send = Send
notifications-reply-sent = Reply sent.
notifications-reply-failed = Couldn’t send the reply
# `action` is the label of one of the notification's own buttons, in the
# device's language (such as "Mark as read").
notifications-action-failed = Couldn’t do “{ $action }”
notifications-dismiss-failed = Couldn’t dismiss it
# A desktop notification's title for a new one: its title (or its app's
# name), then the device's name.
notifications-announce-title = { $title } · { $name }
# Why an action on one failed.
notifications-error-notification_not_found = It’s no longer on the device.
notifications-error-notification_not_repliable = It doesn’t take a reply.
notifications-error-notification_not_dismissable = It can’t be dismissed from here.
notifications-error-unknown_notification_action = It no longer has that button.
# Also when a reply is left empty.
notifications-error-empty_reply = Write a message.

## A phone's calls (src/ui/features/telephony.rs)

# A desktop notification's body while a call rings on a phone; the phone's
# name is its title. `caller` is the contact's name, their number, or
# telephony-unknown-caller.
telephony-incoming-call = Incoming call from { $caller }
# The same, once the call rang out unanswered.
telephony-missed-call = Missed call from { $caller }
telephony-unknown-caller = Unknown caller
# The button on the incoming call's notification that silences the phone.
telephony-mute = Mute
# The device's action (and tray item) while a call rings on it.
telephony-mute-action = Mute ringer
# `name` is the phone's.
telephony-muted = Asked { $name } to mute its ringer.
telephony-mute-failed = Couldn’t mute { $name }
telephony-error-not_ringing = No call is ringing on it now.
# The device's status chip during a call.
telephony-status-ringing = Ringing
telephony-status-talking = On a call

## Browsing a device's files (src/ui/features/browse/)

browse-action = Browse files
# The page's title; `name` is the device's.
browse-title = Files on { $name }
browse-not-shared = { $name } doesn’t share its files.
browse-not-connected = Connect { $name } to browse its files.
# Tooltips of the header's icon buttons.
browse-upload = Upload files
browse-new-folder = New folder
browse-refresh = Refresh
browse-hide-hidden = Hide hidden files
browse-show-hidden = Show hidden files
browse-up = Up
browse-loading-folder = Loading the folder…
browse-connecting = Connecting to the device…
browse-no-storage = The device isn’t sharing any storage.
browse-empty-folder = This folder is empty. Drop files here to upload them.
# The first breadcrumb: the list of the device's storage (internal, SD card).
browse-storage = Storage
# The listing's column headers.
browse-column-name = Name
browse-column-size = Size
browse-column-modified = Modified
# A file's menu.
browse-more = More
browse-preview = Preview
browse-download = Download
browse-rename = Rename
browse-delete = Delete
# `file` is the file's name; the button opens the Transfers page.
browse-downloading = Downloading { $file }
browse-see-transfers = Transfers
# `folder` is the folder's name.
browse-drop-hint = Drop to upload to { $folder }
browse-upload-title = Upload files to { $folder }
# The dialog naming a new folder or a file's new name.
browse-name-label = Name
browse-new-folder-title = New folder
browse-new-folder-confirm = Create
browse-rename-title = Rename
browse-rename-confirm = Rename
browse-name-empty = Enter a name.
browse-name-reserved = That name is reserved.
browse-name-slash = Names can’t contain “/”.
# `name` is the file's or folder's.
browse-delete-folder-title = Delete folder?
browse-delete-folder-body = “{ $name }” and everything in it will be deleted from the device. This can’t be undone.
browse-delete-file-title = Delete file?
browse-delete-file-body = “{ $name }” will be deleted from the device. This can’t be undone.
browse-delete-confirm = Delete
# The image preview.
browse-loading-image = Loading the image…
browse-close-preview = Close
browse-image-unreadable = This image can’t be shown.
# Errors (src/ui/features/browse/describe.rs).
browse-error-file-unreadable = The file couldn’t be read.
browse-error-files_unavailable = The device isn’t sharing its files. In KDE Connect on the device, allow access to files in the Filesystem expose plugin.
# `reason` is the device's own words, in its language.
browse-error-files_unavailable-reason = The device isn’t sharing its files ({ $reason }). In KDE Connect on the device, allow access to files in the Filesystem expose plugin.
browse-error-file_not_found = That file or folder no longer exists.
browse-error-file_exists = There is already a file or folder with that name.
browse-error-file_permission_denied = The device doesn’t allow that.
browse-error-not_a_directory = That isn’t a folder.
browse-error-is_a_directory = Folders can’t be downloaded, only files.
browse-error-invalid_path = That name or location can’t be used.
browse-error-files_failed = The device’s files couldn’t be reached.
browse-error-files_timed_out = The device took too long to answer.
browse-error-files_host_key_mismatch = The device’s file server didn’t prove it is the paired device, so Ferry didn’t connect to it.

## Outside the app: the menu entry, the installer and the system's own
## texts about it (packaging/, through packaging/i18n.sh)
##
## Plain text on one line: no { }, as the packaging scripts copy it as it
## is. Each is shown by the system in the user's language, so a language
## gets them from its `.ftl` when it is packaged.

# The Linux menu entry's generic name, under or instead of "Ferry".
package-generic-name = Device Connection
# The Linux menu entry's tooltip.
package-comment = Pair with your devices and share files and the clipboard over the local network
# Search words for the Linux menu entry: each ends with “;”, and English
# ones may stay so that searching in English still finds the app.
package-keywords = KDE Connect;phone;pair;clipboard;file transfer;
# Why the app wants the local network, in macOS's permission prompt.
package-local-network-usage = Ferry finds and connects to your devices on the local network.
# The checkbox on the Windows installer's last page.
package-start-app = Start Ferry
