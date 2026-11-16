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

Start the machine. Guest IP is under the screen, and under the window title. `.1` is this Mac. If it stays blank, the guest is still booting.

```sh
ssh -i ~/Library/Application\ Support/virtual-machine-manager/ssh/id_ed25519 -o IdentitiesOnly=yes debian@<Guest IP>
```
