# Maintainer: apost <apost@intradatech.com>
pkgname=notif-git
pkgver=r18.709003f
pkgrel=1
pkgdesc="Wayland notification daemon + control center for Hyprland and other wlr-layer-shell compositors"
arch=('x86_64' 'aarch64')
url="https://github.com/adamrpostjr/notif"
license=('MIT')
depends=('wayland' 'gcc-libs' 'glibc')
makedepends=('cargo' 'git')
provides=('notif' 'notifd' 'notifctl')
conflicts=('notif')
source=("$pkgname::git+https://github.com/adamrpostjr/notif.git")
sha256sums=('SKIP')

pkgver() {
    cd "$pkgname"
    printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
    cd "$pkgname"
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --frozen --release --workspace
}

check() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen --release --workspace
}

package() {
    cd "$pkgname"
    install -Dm755 target/release/notifd "$pkgdir/usr/bin/notifd"
    install -Dm755 target/release/notifctl "$pkgdir/usr/bin/notifctl"
    install -Dm644 notifd.service "$pkgdir/usr/lib/systemd/user/notifd.service"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
