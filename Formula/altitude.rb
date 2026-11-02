class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.6.1/altitude-0.6.1-aarch64-apple-darwin.tar.gz"
  sha256 "ff31ff2b0de73e25179675bfe05e362ae90414e494bbff5487a021dcef7a73e1"
  license "BUSL-1.1"
  version "0.6.1"

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
