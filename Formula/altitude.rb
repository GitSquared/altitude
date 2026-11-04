class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.7.1/altitude-0.7.1-aarch64-apple-darwin.tar.gz"
  sha256 "a02025d3723739707533da4ac66cac2b749eb0d93e0a7b7bfe234656923ad315"
  license "BUSL-1.1"
  version "0.7.1"

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
