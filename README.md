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

Start the machine. Guest IP is the bar under the screen, and in the window title. `.1` and `.2` are this Mac. "Looking up…" means the guest is still booting.

If the bar stays on "Looking up…" and the log says `no ARP row`, run `./debug-arp.sh` while the guest is up. It prints the `mac` file, `arp -an`, and why each row is kept or dropped.

```sh
ssh -i ~/Library/Application\ Support/virtual-machine-manager/ssh/id_ed25519 -o IdentitiesOnly=yes debian@<Guest IP>
```
