# Flummox

Your games take up less space and they still work after! Flummox is named for the reaction to how much storage you end up saving. (which in *my* personal experience, is a lot)

Basically, Flummox beats your filesystem into compressing your game files (or whatever else you add to Flummox!) The files are not moved, they are just told to be stored in a different way.

Savings you get back very much depend on what is being compressed. Stuff that is already squeezed, like music, video and those bigfile things, you know them? those can't really be compressed again, so the gains would be very small and not worth the CPU. (nothing ever gets *bigger* though, the filesystem just leaves those parts alone.) Raw textures and plain game data are where the space comes back: one 240 MB Firewatch asset file went down to 162 MB on my drive.

## Get Flummox

Now if that explanation tingled your tism senses(it certainly did mine) to want to try Flummox, then go ahead and download it from [GitHub Releases](https://github.com/bybrooklyn/flummox/releases), you can find a windows installer here powered by good ol Inno Setup (fitgirl uses this btw)

If you use arch(or cachyos) btw and want to download it using the AUR (i promise no virus!)
then just go on ahead and run `paru -S flummox-bin`

For (slightly more) normal linux users, you can get this masterpiece of a software from the archives in [the releases page](https://github.com/bybrooklyn/flummox/releases).

And for our **lovely** MacOS users, you can install Flummox from our homebrew cask.

```sh
brew tap bybrooklyn/flummox
brew install --cask flummox
```

(just keep in mind that macos is a shell right now and needs a bit more work before release)

### And if you are a build-from-source kinda guy(gender neutral):

Arch/Arch based distros:

```sh
git clone https://github.com/bybrooklyn/flummox
cd flummox/packaging && makepkg -si
```

Or if you prefer other distros (and have Rust and FUSE dev files):

```sh
git clone https://github.com/bybrooklyn/flummox
cd flummox
cargo build --release --features gui,pack-mount
```

For our Windows friends:

```powershell
cargo build --release --features gui
```

Anything above here gives you two commands for your favorite terminal emulator, `flummox` for CLI and scripting, and `flummox-gui` for... you won't believe it-- the GUI!

## Is this safe?

Compression changes how the filesystem stores a file, not the file. Every byte
reads back identically, which the tests check by comparing checksums before and
after. Native compression retains the same paths and files. Maximum Space moves the
original into a retained rollback folder when activated. Test the mounted game
before choosing Reclaim original; that explicit action deletes the retained copy.
Restoration then rebuilds ordinary files from the verified store and update layer.

The one case to know about: if your drive has snapshots, rewriting a file
unshares it from its snapshots, so usage can go **up** until those snapshots
expire. Flummox checks and warns before it starts.

## Everything past this point is the smart AI nonsense

- [Using Flummox](docs/usage.md): the window, the command line, and compressing new downloads automatically
- [Status](docs/status.md): what works on each platform, what does not yet, and what the next release adds
- [How it works](docs/how-it-works.md): the mechanism, and benchmarks against other approaches
- [Installation](docs/install.md): packages, upgrades and release builds
- [Jobs and compression design](docs/jobs-and-compression.md): state transitions, recovery and sampling
- [Maximum Space stores](docs/pack-store.md): store commands and format
- [Release acceptance](docs/validation/README.md): what must pass before a tag
- [Security audit](docs/security/2026-10-08-audit.md): what was reviewed, what was fixed, and what is still open
- [Changelog](CHANGELOG.md): what each version changed

## Licence

AGPL-3.0-or-later. See [LICENSE](LICENSE).

## AI disclosure

Development of Flummox is directed by humans and programmed by LLMs, including
Claude, Codex and Muse.
