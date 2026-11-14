class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.9.5/altitude-0.9.5-aarch64-apple-darwin.tar.gz"
  sha256 "97713f96e49cb6abef04d0aa984aa8be7fa547bf889460b509a547a18a2c9599"
  license "BUSL-1.1"
  version "0.9.5"

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
