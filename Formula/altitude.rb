class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.9.7/altitude-0.9.7-aarch64-apple-darwin.tar.gz"
  sha256 "36b8348a8acbe04334033a4af3d129f0e5f335244a78a195c1e15154a819f3bf"
  license "BUSL-1.1"
  version "0.9.7"

  depends_on :macos
  depends_on arch: :arm64

  def install
    # Homebrew stages a single-root archive inside that root, so the binary is either
    # here or one directory down; an archive that renames its root still installs.
    bin.install Dir["altitude", "*/altitude"].first
  end

  test do
    assert_match "altitude v#{version}", shell_output("#{bin}/altitude --version")
  end
end
