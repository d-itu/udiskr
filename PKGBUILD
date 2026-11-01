# Maintainer: Tran Vo Son Tung <sontungexpt@gmail.com>

pkgname=udiskr
pkgver=0.2.0.r22.gbbedf4a
pkgrel=1
pkgdesc="Lightweight UDisks2 wrapper for automatic device mounting"
arch=('x86_64' 'aarch64' 'armv7h' 'i686')
url="https://github.com/sontungexpt/udiskr"
license=('MIT')
depends=('udisks2' 'dbus')
makedepends=('cargo' 'git')
optdepends=(
    'xdg-utils: open mounted directories'
    'libnotify: desktop notifications'
)
source=("git+$url.git")
sha256sums=('SKIP')

# Set up Cargo build directories
export CARGO_HOME="$srcdir/cargo-home"
export CARGO_TARGET_DIR="$srcdir/target"

pkgver() {
    cd "$srcdir/$pkgname"
    git describe --long --tags --match 'v[0-9]*' 2>/dev/null |
        sed -E 's/^v//;s/([^-]*-g)/r\1/;s/-/./g' ||
        printf "0.2.0.r%s.g%s\n" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
    cd "$srcdir/$pkgname"
    cargo fetch --locked --target "$(rustc -Vv | grep host | cut -f2 -d' ')"
}

build() {
    cd "$srcdir/$pkgname"
    cargo build --release --frozen
}

check() {
    cd "$srcdir/$pkgname"
    cargo test --frozen
}

package() {
    cd "$srcdir/$pkgname"

    # Binary
    install -Dm755 "$CARGO_TARGET_DIR/release/$pkgname" "$pkgdir/usr/bin/$pkgname"

    # License
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"

    # Documentation
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
