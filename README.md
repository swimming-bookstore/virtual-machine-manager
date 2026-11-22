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

The `debian` account password is `debian`. The install also writes your host key into that account.

The private key is:

```
~/Library/Application Support/virtual-machine-manager/ssh/id_ed25519
```

```sh
open -R ~/Library/Application\ Support/virtual-machine-manager
```

The app logs in by itself after install. To do it yourself, use the guest address on `192.168.64.0/24` (not `.1`, that is the host):

```sh
ssh -i ~/Library/Application\ Support/virtual-machine-manager/ssh/id_ed25519 debian@192.168.64.2
```
