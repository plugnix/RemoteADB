set -e

PLIST_PATH="/Library/LaunchDaemons/com.plugnix.remoteadb.plist"

cat > "$PLIST_PATH" <<'PLIST_EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.plugnix.remoteadb</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>-c</string>
        <string>[BINARYPATH] host --system --adb-port [ADBPORT][RELAYARGS]</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>remoteadb=info</string>
        <key>PATH</key>
        <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>/var/root</string>
    <key>StandardOutPath</key>
    <string>/var/log/remoteadb.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/remoteadb.log</string>
</dict>
</plist>
PLIST_EOF

echo "starting remoteadb..."
if launchctl list 2>/dev/null | grep -q com.plugnix.remoteadb; then
    launchctl bootout system/com.plugnix.remoteadb || true
    sleep 1
fi

launchctl bootstrap system "$PLIST_PATH"

echo "done"
