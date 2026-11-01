#!/bin/sh
# Packages needed to *build* sls on Ubuntu 22.04+ (plus Rust from rustup.rs).
set -e
sudo apt-get install -y build-essential pkg-config libgtk-4-dev librsvg2-common libglib2.0-bin librsvg2-bin
