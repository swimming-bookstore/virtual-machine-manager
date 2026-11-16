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

Start the machine. The IP is beside Start, and beside the machine name. `.1` is this Mac. If both stay blank, the guest is still booting.

```sh
ssh -i ~/Library/Application\ Support/virtual-machine-manager/ssh/id_ed25519 -o IdentitiesOnly=yes debian@192.168.64.N
```
