PLIST_PATH="/Library/LaunchDaemons/com.plugnix.remoteadb.plist"

launchctl bootout system/com.plugnix.remoteadb 2>/dev/null || launchctl unload "$PLIST_PATH"
rm -f "$PLIST_PATH"
echo "config, key, and ticket left in /etc/remoteadb (remove by hand if unwanted)"
