#!/bin/sh
# The app's translations, for the packages: what the system shows about the
# app outside it (the Linux menu entry, macOS's permission prompts and
# dialogs, the Windows installer) in each language the app ships, from the
# `package-*` messages in i18n/<lang>/ferry.ftl (docs/archive/PLAN_I18N.md).
#
#   i18n.sh languages          the shipped languages, en-US first
#   i18n.sh message LANG KEY   one package-* message
#   i18n.sh desktop FILE       FILE, a .desktop entry, with its GenericName,
#                              Comment and Keywords from en-US and a
#                              Key[xx] line for each other language
#   i18n.sh macos RESOURCES    writes RESOURCES/<name>.lproj/InfoPlist.strings
#                              for each language and prints the names, for
#                              CFBundleLocalizations
#   i18n.sh nsis               an NSIS include: MUI_LANGUAGE for each
#                              language and its LangStrings
#
# The package-* messages are plain text on one line (a unit test,
# `ui::i18n::tests`, checks), so they are copied as they are. A new
# language needs a name for NSIS below; the other names follow from its tag.
set -eu

i18n=$(cd "$(dirname "$0")/../i18n" && pwd)

usage() {
  sed -n '7,16s/^# \{0,1\}//p' "$0" >&2
  exit 2
}

languages() {
  echo en-US
  for dir in "$i18n"/*/; do
    language=$(basename "$dir")
    [ "$language" = en-US ] || echo "$language"
  done
}

# message LANG KEY
message() {
  file=$i18n/$1/ferry.ftl
  value=$(awk -v key="$2" '
    index($0, key " =") == 1 { sub(/^[^=]*= ?/, ""); print; found = 1; exit }
    END { if (!found) exit 1 }' "$file") || {
    echo "i18n.sh: $file has no $2" >&2
    exit 1
  }
  if [ -z "$value" ]; then
    echo "i18n.sh: $2 in $file isn't on one line" >&2
    exit 1
  fi
  printf '%s\n' "$value"
}

# The locale a .desktop entry names a language by: zh_CN, de.
desktop_locale() {
  echo "$1" | tr - _
}

# The .lproj macOS looks for: the development region (en) for en-US,
# script-based names for Chinese, the tag itself otherwise.
lproj() {
  case $1 in
  en-US) echo en ;;
  zh-CN | zh-SG | zh-Hans*) echo zh-Hans ;;
  zh-TW | zh-HK | zh-MO | zh-Hant*) echo zh-Hant ;;
  *) echo "$1" ;;
  esac
}

# NSIS's name for a language (Contrib/Language files/<name>.nlf).
nsis_language() {
  case $1 in
  en-US) echo English ;;
  de | de-*) echo German ;;
  zh-CN | zh-SG | zh-Hans*) echo SimpChinese ;;
  zh-TW | zh-HK | zh-MO | zh-Hant*) echo TradChinese ;;
  fr | fr-*) echo French ;;
  es | es-*) echo Spanish ;;
  it | it-*) echo Italian ;;
  ja | ja-*) echo Japanese ;;
  ko | ko-*) echo Korean ;;
  nl | nl-*) echo Dutch ;;
  pl | pl-*) echo Polish ;;
  pt-BR) echo PortugueseBR ;;
  pt | pt-*) echo Portuguese ;;
  ru | ru-*) echo Russian ;;
  *)
    echo "i18n.sh: no NSIS language for $1; add it to nsis_language" >&2
    exit 1
    ;;
  esac
}

# A .desktop string value: \ and line breaks escaped (there are none).
desktop_value() {
  printf '%s\n' "$1" | sed 's/\\/\\\\/g'
}

desktop() {
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
    GenericName=*) key=GenericName message=package-generic-name ;;
    Comment=*) key=Comment message=package-comment ;;
    Keywords=*) key=Keywords message=package-keywords ;;
    *)
      printf '%s\n' "$line"
      continue
      ;;
    esac
    # Assigned first, so that set -e stops at a missing message.
    value=$(message en-US $message)
    printf '%s=%s\n' "$key" "$(desktop_value "$value")"
    for language in $(languages); do
      [ "$language" = en-US ] && continue
      value=$(message "$language" $message)
      printf '%s[%s]=%s\n' "$key" "$(desktop_locale "$language")" \
        "$(desktop_value "$value")"
    done
  done <"$1"
}

# A .strings value, quoted: \ and " escaped.
strings_value() {
  printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
}

macos() {
  for language in $(languages); do
    name=$(lproj "$language")
    value=$(message "$language" package-local-network-usage)
    mkdir -p "$1/$name.lproj"
    printf '/* From i18n/%s/ferry.ftl (packaging/i18n.sh). */\n"NSLocalNetworkUsageDescription" = %s;\n' \
      "$language" "$(strings_value "$value")" >"$1/$name.lproj/InfoPlist.strings"
    echo "$name"
  done
}

# An NSIS string: $ and " escaped.
nsis_value() {
  printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\$/$$/g' -e 's/"/$\\"/g')"
}

nsis() {
  echo "; From i18n/*/ferry.ftl (packaging/i18n.sh). The first is the default."
  for language in $(languages); do
    name=$(nsis_language "$language")
    echo "!insertmacro MUI_LANGUAGE \"$name\""
  done
  for language in $(languages); do
    name=$(nsis_language "$language")
    value=$(message "$language" package-start-app)
    id=LANG_$(echo "$name" | tr '[:lower:]' '[:upper:]')
    echo "LangString package_start_app \${$id} $(nsis_value "$value")"
  done
}

[ $# -ge 1 ] || usage
command=$1
shift
case $command in
languages) [ $# -eq 0 ] || usage && languages ;;
message) [ $# -eq 2 ] || usage && message "$1" "$2" ;;
desktop) [ $# -eq 1 ] || usage && desktop "$1" ;;
macos) [ $# -eq 1 ] || usage && macos "$1" ;;
nsis) [ $# -eq 0 ] || usage && nsis ;;
*) usage ;;
esac
