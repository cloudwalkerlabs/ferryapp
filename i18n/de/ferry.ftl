# Ferry's desktop app, in German.
#
# Written by an AI agent from i18n/en-US/ferry.ftl and not yet reviewed by
# a native speaker: corrections are welcome as pull requests. The comments
# describing each message are in en-US/ferry.ftl; keys and arguments must
# match that file (`ui::i18n::tests` checks). See docs/archive/PLAN_I18N.md.

## Files dropped on the window or the tray icon (src/ui/drops.rs)

drop-send-to-header =
    { $count ->
        [one] { $count } Datei senden an:
       *[other] { $count } Dateien senden an:
    }
drop-no-recipients = Kein gekoppeltes Gerät kann Dateien empfangen
drop-send-failed = Senden fehlgeschlagen
drop-folders-refused = Nur Dateien können gesendet werden, keine Ordner.

## Files dropped on the window: the hint and the chooser (src/ui/overlay/drop.rs)

drop-choose-hint = Irgendwo ablegen, um ein Gerät zu wählen
drop-chooser-title-one = Datei senden
drop-chooser-title =
    { $count ->
        [one] { $count } Datei senden
       *[other] { $count } Dateien senden
    }
drop-chooser-prompt = Wähle das Gerät, an das gesendet wird:
drop-chooser-no-devices = Kein gekoppeltes Gerät ist verbunden und kann Dateien empfangen.

## Dialogs (src/ui/overlay/dialog.rs; also the drop chooser)

dialog-cancel = Abbrechen
dialog-counter = { $count }/{ $max }

## An incoming pairing request (src/ui/overlay/incoming.rs)

incoming-title = Kopplungsanfrage
incoming-body = { $name } möchte sich mit diesem Computer koppeln. Nimm nur an, wenn dort derselbe Code angezeigt wird:
incoming-queued =
    { $count ->
        [one] { $count } weitere Anfrage wartet
       *[other] { $count } weitere Anfragen warten
    }
incoming-reject = Ablehnen
incoming-accept = Annehmen

## Errors, keyed by the code the HTTP API reports (src/ui/error.rs)

error-daemon_unavailable = Ferry antwortet nicht.
error-unauthorized = Ferry hat das Zugriffstoken dieser App abgelehnt.
error-device_not_found = Dieses Gerät ist nicht mehr bekannt.
error-device_not_connected = Das Gerät ist gerade nicht verbunden.
error-already_paired = Das Gerät ist bereits gekoppelt.
error-pairing_in_progress = Eine Kopplung mit diesem Gerät läuft bereits.
error-pairing_not_found = Diese Kopplungsanfrage gibt es nicht mehr.
error-invalid_pairing_state = Diese Kopplungsanfrage ist nicht mehr aktiv.
error-device_not_paired = Das Gerät ist nicht gekoppelt.
error-unsupported_by_peer = Das Gerät unterstützt das nicht.
error-invalid_file_name = Eine Datei mit diesem Namen kann nicht gesendet werden.
error-transfer_too_large = Die Datei ist zu groß zum Senden.
error-transfer_not_found = Diese Übertragung gibt es nicht mehr.
error-invalid_transfer_state = Diese Übertragung ist bereits beendet.
error-request_timeout = Ferry hat zu lange für eine Antwort gebraucht.
error-invalid_device_name = Verwende 1 bis 32 Zeichen, ohne . , : ; ! ? ( ) [ ] < > oder Anführungszeichen.
error-invalid_download_dir = Dieser Ordner kann nicht für Downloads verwendet werden.
error-invalid_address = Gib eine IPv4-Adresse ein, etwa 192.168.1.20.
error-address_unreachable = Unter dieser Adresse hat kein Gerät geantwortet. Prüfe, ob dort Ferry oder KDE Connect läuft.
error-too_many_addresses = Ein Gerät kann höchstens 8 Adressen speichern.
error-unknown = Etwas ist schiefgelaufen ({ $code }).
error-send-file-failed = { $file } konnte nicht gesendet werden: { $reason }
error-send-files-failed =
    { $count ->
        [one] { $count } Datei konnte nicht gesendet werden: { $reason }
       *[other] { $count } Dateien konnten nicht gesendet werden: { $reason }
    }
error-upload-file-failed = { $file } konnte nicht hochgeladen werden: { $reason }
error-upload-files-failed =
    { $count ->
        [one] { $count } Datei konnte nicht hochgeladen werden: { $reason }
       *[other] { $count } Dateien konnten nicht hochgeladen werden: { $reason }
    }

## Starting the app (src/ui/launch.rs, src/ui/pages/startup.rs)

app-window-title = Ferry
startup-starting = Ferry wird gestartet …
startup-failed = Ferry konnte nicht starten.
    { $error }

## The shell: the window's own messages and dialogs (src/ui/mod.rs,
## src/ui/shell.rs, src/ui/actions.rs)

shell-notification-toast = { $title }: { $body }
shell-unknown-device = Dieses Gerät ist nicht mehr bekannt.
shell-unpair-title = Kopplung aufheben?
shell-unpair-body = { $name } muss erneut gekoppelt werden, bevor es wieder etwas mit diesem Computer austauschen kann.
shell-unpair-confirm = Kopplung aufheben
shell-open-failed = { $path } konnte nicht geöffnet werden
shell-open-link-failed = { $url } konnte nicht geöffnet werden
shell-start-on-login-failed = Der Start bei der Anmeldung konnte nicht geändert werden.
shell-add-by-address-title = Über IP-Adresse hinzufügen
shell-add-by-address-label = IP-Adresse
shell-add-by-address-helper = Auf dem Gerät muss Ferry oder KDE Connect laufen. Ferry verbindet sich, koppelt und merkt sich die Adresse.
shell-add-by-address-confirm = Hinzufügen
shell-address-add-title = Adresse hinzufügen
shell-address-edit-title = Adresse bearbeiten
shell-address-body = Ferry versucht diese Adresse, wenn { $name } im Netzwerk nicht gefunden wird.
shell-address-save = Speichern
shell-rename-title = Gerätename
shell-rename-helper = So erscheint dieser Computer auf deinen anderen Geräten
shell-rename-confirm = Speichern
shell-download-dir-title = Empfangene Dateien speichern in
shell-autostart-comment = Ferry im Infobereich starten

## The tray menu (src/ui/background.rs)

tray-open = Ferry öffnen
tray-no-paired-devices = Keine gekoppelten Geräte
tray-no-devices-connected = Keine Geräte verbunden
tray-show-details = Details anzeigen
tray-settings = Einstellungen
tray-about = Über Ferry
tray-quit = Beenden

## Desktop notifications (src/ui/background.rs, src/ui/desktop/notify.rs)

notify-pairing-title = Kopplungsanfrage
notify-pairing-body = { $name } möchte sich mit diesem Computer koppeln.
notify-file-received-title = Datei empfangen
notify-file-received-body = { $file } von { $name }
notify-open = Öffnen

## Widgets the pages share (src/ui/widgets.rs)

widget-back = Zurück
widget-retry = Erneut versuchen
widget-size-bytes =
    { $count ->
        [one] { $count } Byte
       *[other] { $count } Bytes
    }
widget-size-kb = { $size } KB
widget-size-mb = { $size } MB
widget-size-gb = { $size } GB
widget-size-tb = { $size } TB

## The device list, the home page (src/ui/pages/devices.rs)

devices-title = Geräte
devices-settings = Einstellungen
devices-transfers = Übertragungen
devices-this-computer = Dieser Computer: { $name }
devices-add = Gerät hinzufügen
devices-loading = Geräte werden geladen …
devices-empty = Noch keine gekoppelten Geräte
devices-find = Gerät zum Koppeln suchen
device-reachability-connected = Verbunden
device-reachability-nearby = In der Nähe
device-reachability-unavailable = Nicht erreichbar

## One device's page (src/ui/pages/device.rs)

device-title = Gerät
device-unknown = Dieses Gerät ist nicht mehr bekannt.
device-recent-transfers = Letzte Übertragungen
device-see-all = Alle anzeigen
device-id = Geräte-ID
device-type = Typ
device-protocol-version = Protokollversion
device-type-desktop = Desktop
device-type-laptop = Laptop
device-type-phone = Smartphone
device-type-tablet = Tablet
device-type-tv = Fernseher
device-unpair = Kopplung aufheben
device-addresses-title = Adressen
device-addresses-detail = Ferry versucht diese Adressen, wenn das Gerät im Netzwerk nicht gefunden wird.
device-addresses-none = Keine Adressen gespeichert
device-address-add = Adresse hinzufügen
device-address-edit = Bearbeiten
device-address-remove = Entfernen

## Adding a device (src/ui/pages/add_device.rs)

add-device-title = Gerät hinzufügen
add-device-scan = Erneut suchen
add-device-instructions = Öffne Ferry oder KDE Connect auf dem anderen Gerät und achte darauf, dass beide im selben Netzwerk sind.
add-device-none-found = Keine Geräte gefunden
add-device-pairing = Kopplung läuft
add-device-not-connected = Nicht verbunden
add-device-pair = Koppeln
add-device-by-address = Über IP-Adresse hinzufügen
add-device-by-address-detail = Für Netzwerke, in denen das Gerät nicht von selbst erscheint
add-device-connecting = Verbinde mit { $address } …
add-device-connecting-detail = Warte auf die Antwort des Geräts
add-device-cancel = Abbrechen

## Pairing with a device (src/ui/pages/pairing.rs)

pairing-title = Kopplung
pairing-unknown = Diese Kopplungsanfrage gibt es nicht mehr.
pairing-waiting = Warten auf { $name }
pairing-waiting-detail = Prüfe, ob { $name } denselben Code anzeigt, und nimm die Anfrage dort an.
pairing-accepted = Mit { $name } gekoppelt
pairing-rejected = Kopplung abgelehnt
pairing-rejected-detail = Die Anfrage wurde abgelehnt oder abgebrochen.
pairing-expired = Zeitüberschreitung der Anfrage
pairing-expired-detail = { $name } hat nicht rechtzeitig geantwortet.
pairing-failed = Kopplung fehlgeschlagen
pairing-failed-detail = Die Verbindung zu { $name } wurde unterbrochen.
pairing-cancel = Abbrechen
pairing-done = Fertig
pairing-close = Schließen
pairing-try-again = Erneut versuchen

## File transfers (src/ui/pages/transfers.rs)

transfers-title = Übertragungen
transfers-loading = Übertragungen werden geladen …
transfers-empty = Noch keine Übertragungen
transfers-from = Von { $name } · { $status }
transfers-to = An { $name } · { $status }
transfers-cancel = Abbrechen
transfers-open-file = Datei öffnen
transfers-open-folder = Ordner öffnen
transfers-queued = Wartet
transfers-connecting = Verbindung wird hergestellt
transfers-progress = { $done } von { $total }
transfers-cancelled = Abgebrochen
transfers-failed = Fehlgeschlagen
transfers-failed-connection_failed = Fehlgeschlagen: Verbindung unterbrochen
transfers-failed-timed_out = Fehlgeschlagen: Zeitüberschreitung
transfers-failed-unavailable = Fehlgeschlagen: vom Empfänger abgelehnt
transfers-failed-protocol_error = Fehlgeschlagen: Das Gerät hat etwas Unerwartetes gesendet

## Settings (src/ui/pages/settings.rs)

settings-title = Einstellungen
settings-loading = Einstellungen werden geladen …
settings-device-name = Gerätename
settings-download-dir = Empfangene Dateien speichern in
settings-close-to-tray = Weiterlaufen, wenn das Fenster geschlossen wird
settings-close-to-tray-detail = Im Infobereich bleiben, damit Geräte diesen Computer weiterhin erreichen
settings-start-on-login = Bei der Anmeldung starten
settings-start-on-login-detail = Im Infobereich öffnen, bereit für deine Geräte
settings-language = Sprache
settings-language-detail = Für das Fenster, das Menü im Infobereich und die Benachrichtigungen der App
settings-language-system = Systemstandard
settings-language-own-name = Deutsch
settings-appearance = Erscheinungsbild
settings-appearance-detail = Hell oder dunkel, oder wie dein System
settings-appearance-system = Systemstandard
settings-appearance-light = Hell
settings-appearance-dark = Dunkel
settings-cli = Zugriff über die Befehlszeile
settings-cli-detail = ferry-cli darf diese App steuern
settings-cli-setup-hint = ferry-cli auf diesem Computer findet die App von selbst. Anderswo, etwa in einem Skript unter einem anderen Benutzer, füge zuerst dies in dessen Shell ein:
settings-cli-copy-setup = Einrichtung kopieren
settings-cli-copy-token = Token kopieren
settings-cli-new-token = Neues Token
settings-cli-installed-at = ferry-cli ist installiert unter
settings-cli-not-listening = ferry-cli kann die App nicht erreichen: { $error }
settings-cli-change-failed = Der Zugriff über die Befehlszeile konnte nicht geändert werden: { $error }
settings-cli-setup-copied = Einrichtung kopiert
settings-cli-token-copied = Token kopiert
settings-about = Über Ferry
settings-version = Version { $version }

## About (src/ui/pages/about.rs); the app's name, Ferry, isn't translated

about-title = Über
about-version = Version { $version }
about-description = Ein KDE-Connect-Client für macOS, Linux und Windows.
about-author = Entwickelt von Fanchao
about-source = Quellcode
about-support = Entwicklung unterstützen
about-support-detail = Auf GitHub sponsern
about-licenses = Open-Source-Lizenzen
about-licenses-detail = Die Software, auf der Ferry aufbaut

## Ping (src/ui/features/ping.rs)

ping-action = Anpingen
ping-received = Ping!
ping-sent = { $name } angepingt.
ping-failed = { $name } konnte nicht angepingt werden

## Find my phone (src/ui/features/findmyphone.rs)

findmyphone-action = Klingeln lassen
findmyphone-sent = { $name } soll klingeln.
findmyphone-failed = { $name } konnte nicht zum Klingeln gebracht werden

## Battery (src/ui/features/battery.rs)

battery-charge = { $charge } %

## Connectivity (src/ui/features/connectivity.rs)

connectivity-unknown-network = Mobilfunk

## Sending files (src/ui/features/share.rs)

share-action = Dateien senden
share-drop-hint = Ablegen, um an { $name } zu senden
share-pick-title = Dateien an { $name } senden
share-failed = Senden an { $name } fehlgeschlagen
share-error-file-unreadable = Die Datei konnte nicht gelesen werden.
share-text-action = Text senden
share-text-title = Text oder Link senden
share-text-to = An { $name }
share-text-label = Text oder Link
share-text-send = Senden
share-error-share_empty = Gib den Text oder Link ein, der gesendet werden soll.
share-error-share_too_large = Der Text ist zu lang zum Senden.
share-text-sent = Text an { $name } gesendet.
share-link-sent = Link an { $name } gesendet.
share-text-copied = Geteilter Text in die Zwischenablage kopiert.
share-text-not-copied = Geteilter Text war zu lang für die Zwischenablage.
share-link-opened = Geteilter Link im Browser geöffnet.
share-link-failed = Geteilter Link konnte nicht im Browser geöffnet werden.

## The clipboard (src/ui/features/clipboard.rs)

clipboard-action = Zwischenablage senden
clipboard-sync = Zwischenablage synchronisieren
clipboard-sync-detail = Kopierten Text mit gekoppelten Geräten teilen
clipboard-sent = Zwischenablage an { $name } gesendet.
clipboard-failed = Zwischenablage konnte nicht an { $name } gesendet werden
clipboard-error-clipboard_empty = In der Zwischenablage ist kein Text zum Senden.
clipboard-error-clipboard_text_too_large = Der Text in der Zwischenablage ist zu lang zum Senden.

## A device's notifications (src/ui/features/notifications.rs)

notifications-action =
    { $count ->
        [0] Benachrichtigungen
       *[other] Benachrichtigungen ({ $count })
    }
notifications-title = Benachrichtigungen
notifications-offline = { $name } ist nicht verbunden.
notifications-offline-detail = Sobald es verbunden ist, erscheinen seine Benachrichtigungen hier.
notifications-empty = Keine Benachrichtigungen von { $name }.
notifications-empty-detail = Erlaube KDE Connect auf dem Smartphone, Benachrichtigungen zu lesen, und wähle aus, welche Apps sie teilen.
notifications-sync = Benachrichtigungen anzeigen
notifications-sync-detail = Die Benachrichtigungen, die { $name } teilt, hier auflisten und melden.
notifications-sync-failed = Die Einstellung konnte nicht geändert werden
notifications-reply = Antworten
notifications-dismiss = Verwerfen
notifications-reply-title = Antworten
notifications-reply-to = An „{ $title }“
notifications-reply-label = Nachricht
notifications-reply-send = Senden
notifications-reply-sent = Antwort gesendet.
notifications-reply-failed = Die Antwort konnte nicht gesendet werden
notifications-action-failed = „{ $action }“ konnte nicht ausgeführt werden
notifications-dismiss-failed = Sie konnte nicht verworfen werden
notifications-announce-title = { $title } · { $name }
notifications-error-notification_not_found = Sie ist nicht mehr auf dem Gerät.
notifications-error-notification_not_repliable = Auf sie kann nicht geantwortet werden.
notifications-error-notification_not_dismissable = Sie kann von hier aus nicht verworfen werden.
notifications-error-unknown_notification_action = Diese Schaltfläche gibt es nicht mehr.
notifications-error-empty_reply = Schreib eine Nachricht.

## A phone's calls (src/ui/features/telephony.rs)

telephony-incoming-call = Eingehender Anruf von { $caller }
telephony-missed-call = Verpasster Anruf von { $caller }
telephony-unknown-caller = Unbekannter Anrufer
telephony-mute = Stummschalten
telephony-mute-action = Klingelton stummschalten
telephony-muted = { $name } soll den Klingelton stummschalten.
telephony-mute-failed = { $name } konnte nicht stummgeschaltet werden
telephony-error-not_ringing = Gerade klingelt dort kein Anruf.
telephony-status-ringing = Klingelt
telephony-status-talking = Im Gespräch

## Browsing a device's files (src/ui/features/browse/)

browse-action = Dateien durchsuchen
browse-title = Dateien auf { $name }
browse-not-shared = { $name } gibt seine Dateien nicht frei.
browse-not-connected = Verbinde { $name }, um seine Dateien zu durchsuchen.
browse-upload = Dateien hochladen
browse-new-folder = Neuer Ordner
browse-refresh = Aktualisieren
browse-hide-hidden = Versteckte Dateien ausblenden
browse-show-hidden = Versteckte Dateien anzeigen
browse-up = Nach oben
browse-loading-folder = Ordner wird geladen …
browse-connecting = Verbindung zum Gerät wird hergestellt …
browse-no-storage = Das Gerät gibt keinen Speicher frei.
browse-empty-folder = Dieser Ordner ist leer. Lege Dateien hier ab, um sie hochzuladen.
browse-storage = Speicher
browse-column-name = Name
browse-column-size = Größe
browse-column-modified = Geändert
browse-more = Mehr
browse-preview = Vorschau
browse-download = Herunterladen
browse-rename = Umbenennen
browse-delete = Löschen
browse-downloading = { $file } wird heruntergeladen
browse-see-transfers = Übertragungen
browse-drop-hint = Ablegen, um nach { $folder } hochzuladen
browse-upload-title = Dateien nach { $folder } hochladen
browse-name-label = Name
browse-new-folder-title = Neuer Ordner
browse-new-folder-confirm = Erstellen
browse-rename-title = Umbenennen
browse-rename-confirm = Umbenennen
browse-name-empty = Gib einen Namen ein.
browse-name-reserved = Dieser Name ist reserviert.
browse-name-slash = Namen dürfen kein „/“ enthalten.
browse-delete-folder-title = Ordner löschen?
browse-delete-folder-body = „{ $name }“ und alles darin wird vom Gerät gelöscht. Das kann nicht rückgängig gemacht werden.
browse-delete-file-title = Datei löschen?
browse-delete-file-body = „{ $name }“ wird vom Gerät gelöscht. Das kann nicht rückgängig gemacht werden.
browse-delete-confirm = Löschen
browse-loading-image = Bild wird geladen …
browse-close-preview = Schließen
browse-image-unreadable = Dieses Bild kann nicht angezeigt werden.
browse-error-file-unreadable = Die Datei konnte nicht gelesen werden.
browse-error-files_unavailable = Das Gerät gibt seine Dateien nicht frei. Erlaube in KDE Connect auf dem Gerät den Zugriff auf Dateien im Plugin „Dateisystem freigeben“.
browse-error-files_unavailable-reason = Das Gerät gibt seine Dateien nicht frei ({ $reason }). Erlaube in KDE Connect auf dem Gerät den Zugriff auf Dateien im Plugin „Dateisystem freigeben“.
browse-error-file_not_found = Diese Datei oder dieser Ordner existiert nicht mehr.
browse-error-file_exists = Es gibt bereits eine Datei oder einen Ordner mit diesem Namen.
browse-error-file_permission_denied = Das Gerät erlaubt das nicht.
browse-error-not_a_directory = Das ist kein Ordner.
browse-error-is_a_directory = Ordner können nicht heruntergeladen werden, nur Dateien.
browse-error-invalid_path = Dieser Name oder Ort kann nicht verwendet werden.
browse-error-files_failed = Die Dateien des Geräts waren nicht erreichbar.
browse-error-files_timed_out = Das Gerät hat zu lange für eine Antwort gebraucht.
browse-error-files_host_key_mismatch = Der Dateiserver des Geräts konnte nicht nachweisen, dass er das gekoppelte Gerät ist, deshalb hat Ferry keine Verbindung hergestellt.

## Outside the app: the menu entry, the installer and the system's own
## texts about it (packaging/, through packaging/i18n.sh)
##
## Plain text on one line: no { }, as the packaging scripts copy it as it
## is.

package-generic-name = Geräteverbindung
package-comment = Mit deinen Geräten koppeln und Dateien und die Zwischenablage im lokalen Netzwerk teilen
package-keywords = KDE Connect;Smartphone;Handy;koppeln;Zwischenablage;Dateiübertragung;phone;pair;clipboard;file transfer;
package-local-network-usage = Ferry findet deine Geräte im lokalen Netzwerk und verbindet sich mit ihnen.
package-start-app = Ferry starten
