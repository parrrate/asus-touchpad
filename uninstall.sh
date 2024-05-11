if [[ $(id -u) != 0 ]]
then
    echo "run as root"
    exit 1
fi

systemctl stop asus-touchpad
systemctl disable asus-touchpad
rm /usr/share/asus-touchpad/asus-touchpad
rm /etc/systemd/system/asus-touchpad.service
