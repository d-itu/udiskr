
# Maintainer: Tran Vo Son TUng sontungexpt@gmail.com

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

# Keep Cargo files inside the build directory.
export CARGO_HOME="$srcdir/cargo-home"
export CARGO_TARGET_DIR="$srcdir/target"

pkgver() {
    cd "$srcdir/$pkgname"

    local version

    version=$(git describe --long --tags --match 'v[0-9]*' 2>/dev/null) ||
        version="v0.2.0-$(git rev-list --count HEAD)-g$(git rev-parse --short HEAD)"

    printf '%s\n' "$version" |
        sed -E 's/^v//; s/-([0-9]+)-g([0-9a-f]+)$/.r\1.g\2/; s/-/./g'
}

prepare() {
    cd "$srcdir/$pkgname"
    CARGO_HOME="$srcdir/cargo-home" \
    CARGO_TARGET_DIR="$srcdir/target" \
        cargo fetch --locked
}

build() {
    cd "$srcdir/$pkgname"
    CARGO_HOME="$srcdir/cargo-home" \
    CARGO_TARGET_DIR="$srcdir/target" \
        cargo build --release --frozen
}

check() {
    cd "$srcdir/$pkgname"
    CARGO_HOME="$srcdir/cargo-home" \
    CARGO_TARGET_DIR="$srcdir/target" \
        cargo test --frozen
}

package() {
    install -Dm755 "$srcdir/target/release/$pkgname" \
        "$pkgdir/usr/bin/$pkgname"

    install -Dm644 "$srcdir/$pkgname/LICENSE" \
        "$pkgdir/usr/share/licenses/$pkgname/LICENSE"

    install -Dm644 "$srcdir/$pkgname/README.md" \
        "$pkgdir/usr/share/doc/$pkgname/README.md"
}
