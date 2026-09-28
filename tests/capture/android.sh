#!/usr/bin/env bash
# Runs inside reactivecircus/android-emulator-runner: installs an APK, opens the
# app's "add subscription" deep link pointing at capture_server.py on the host
# (10.0.2.2 from the emulator), confirms dialogs and saves screenshots and UI
# dumps next to the captured requests.
#   APK=Happ.apk LINK='happ://add/http://10.0.2.2:18080/sub/abc' OUT=captures ./android.sh
set -u
out=${OUT:-captures}
mkdir -p "$out"
shot=0
snap() {
  shot=$((shot + 1))
  adb exec-out screencap -p >"$out/screen-$shot-$1.png" || true
  adb shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 && adb pull /sdcard/ui.xml "$out/ui-$shot-$1.xml" >/dev/null 2>&1 || true
}
# Taps the first element whose text matches (case-insensitive) one of the words.
tap() {
  adb shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 || return 1
  xml=$(adb exec-out cat /sdcard/ui.xml)
  for word in "$@"; do
    bounds=$(printf '%s' "$xml" | tr '>' '\n' | grep -i "text=\"$word\"" | grep -o 'bounds="[^"]*"' | head -1)
    [ -n "$bounds" ] || continue
    read -r x1 y1 x2 y2 <<<"$(printf '%s' "$bounds" | tr -c '0-9' ' ')"
    adb shell input tap $(((x1 + x2) / 2)) $(((y1 + y2) / 2))
    echo "tapped '$word'"
    return 0
  done
  return 1
}

aapt=$(ls "$ANDROID_HOME"/build-tools/*/aapt 2>/dev/null | tail -1)
[ -n "$aapt" ] && "$aapt" dump badging "$APK" | grep -E "^package:|native-code|sdkVersion" | tee "$out/apk-info.txt"
pkg=$(grep -o "package: name='[^']*'" "$out/apk-info.txt" 2>/dev/null | cut -d"'" -f2)
adb install -r -g "$APK" 2>&1 | tail -2
adb shell getprop ro.product.model >"$out/device.txt"
adb shell getprop ro.build.version.release >>"$out/device.txt"
adb shell settings get secure android_id >>"$out/device.txt"

[ -n "$pkg" ] && adb shell monkey -p "$pkg" -c android.intent.category.LAUNCHER 1 >/dev/null 2>&1
sleep 15
snap launched
for _ in 1 2 3 4; do
  tap "Allow" "OK" "Accept" "Agree" "Continue" "Next" "Skip" "Разрешить" "Принять" "Продолжить" "Далее" "Пропустить" || break
  sleep 3
done
snap onboarding

adb shell am start -a android.intent.action.VIEW -d "$LINK" 2>&1 | tail -1
sleep 12
snap deeplink
for _ in 1 2 3; do
  tap "Add" "OK" "Import" "Yes" "Добавить" "Импорт" "Да" || break
  sleep 8
done
snap added
sleep 20
snap final
ls -la "$out"
