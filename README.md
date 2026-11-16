# Virtual Machine Manager

Mac app that installs Debian in a virtual machine.

Needs macOS 14 and the Xcode command line tools.

## Build

```sh
cargo build --release
./build-macos.sh
open virtual-machine-manager.app
```

## Log in

There is no password. The install writes your host key into the `debian` account.

The private key is:

```
~/Library/Application Support/virtual-machine-manager/ssh/id_ed25519
```

The app logs in by itself after install. To do it yourself, use the guest address on `192.168.64.0/24` (not `.1`, that is the host):

```sh
ssh -i ~/Library/Application\ Support/virtual-machine-manager/ssh/id_ed25519 debian@192.168.64.2
```
