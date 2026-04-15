class Ccrouter < Formula
  desc "Lightweight CLI proxy to route Claude Code to any LLM provider"
  homepage "https://github.com/guo/ccrouter"
  version "0.1.8"
  license "MIT"

  on_macos do
    url "https://github.com/guo/ccrouter/releases/download/v#{version}/ccrouter-v#{version}-universal-apple-darwin.tar.gz"
    sha256 "2580444758448abe515133938556b9975abc90d73b14d04d4a249151c97171c5"
  end

  on_linux do
    on_intel do
      url "https://github.com/guo/ccrouter/releases/download/v#{version}/ccrouter-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "dba357f26cad6681dc15ff7676f4ff52356482f831377497d36ab9ff3d7e3d08"
    end
    on_arm do
      url "https://github.com/guo/ccrouter/releases/download/v#{version}/ccrouter-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "b6ed8e4429a3b1195655e9fd1f013b989b5c5fcd3b3d08a2a4bc97c742f4d7bb"
    end
  end

  def install
    bin.install "ccrouter"
  end

  test do
    assert_match "ccrouter", shell_output("#{bin}/ccrouter --version")
  end
end
