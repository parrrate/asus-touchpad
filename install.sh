if [[ $(id -u) != 0 ]]
then
    echo "run as root"
    exit 1
fi

modprobe i2c-dev
mkdir -p /usr/share/asus-touchpad
cp ./target/release/asus-touchpad /usr/share/asus-touchpad/asus-touchpad
cp ./asus-touchpad.service /etc/systemd/system/asus-touchpad.service
systemctl enable asus-touchpad
systemctl start asus-touchpad
