# shellcheck shell=sh
# Sourced by scripts/build-app.sh and scripts/release/lib.sh before anything is compiled, so built
# files never name a folder of this computer. Compilers keep source paths in what they build (panic
# locations, assertion messages, debug info); the home folder in those paths becomes `~`, so no
# released file holds a user's name. Everything these scripts start inherits it: cortex-pack, cargo,
# a plugin's build.sh, CMake (the plugins' build.sh pass CFLAGS / CXXFLAGS to it), the cc and cmake
# crates. The Linux builds in Docker see no user's home. The release scripts also refuse any file
# holding a name of RELEASE_FORBIDDEN (.env.release).
#
# It counts in the digest of every part built with Rust (scripts/release/digest.mjs), as
# rust-toolchain.toml does: changing it rebuilds those parts.
case " ${RUSTFLAGS:-} " in
  *" --remap-path-prefix=$HOME=~ "*) ;;
  *) RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=~" ;;
esac
case " ${CFLAGS:-} " in
  *" -ffile-prefix-map=$HOME=~ "*) ;;
  *) CFLAGS="${CFLAGS:+$CFLAGS }-ffile-prefix-map=$HOME=~" ;;
esac
case " ${CXXFLAGS:-} " in
  *" -ffile-prefix-map=$HOME=~ "*) ;;
  *) CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-ffile-prefix-map=$HOME=~" ;;
esac
export RUSTFLAGS CFLAGS CXXFLAGS
