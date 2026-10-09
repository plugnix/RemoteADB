
systemctl stop remoteadb.service
systemctl disable remoteadb.service
rm -f /etc/systemd/system/remoteadb.service
systemctl daemon-reload
echo "config, key, and ticket left in /etc/remoteadb (remove by hand if unwanted)"
