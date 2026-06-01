# Homebrew formula for recursive-agent
# Hosted in a tap: github.com/recursive-agent/homebrew-recursive
#
# Install:
#   brew tap recursive-agent/recursive
#   brew install recursive
#
# Or one-liner:
#   brew install recursive-agent/recursive/recursive

class Recursive < Formula
  desc "Minimal, orthogonal, self-improving coding agent kernel"
  homepage "https://github.com/recursive-agent/recursive"
  version "0.6.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/recursive-agent/recursive/releases/download/v#{version}/recursive-aarch64-apple-darwin.tar.gz"
      sha256 "" # filled by release automation
    end
    on_intel do
      url "https://github.com/recursive-agent/recursive/releases/download/v#{version}/recursive-x86_64-apple-darwin.tar.gz"
      sha256 "" # filled by release automation
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/recursive-agent/recursive/releases/download/v#{version}/recursive-aarch64-unknown-linux-musl.tar.gz"
      sha256 "" # filled by release automation
    end
    on_intel do
      url "https://github.com/recursive-agent/recursive/releases/download/v#{version}/recursive-x86_64-unknown-linux-musl.tar.gz"
      sha256 "" # filled by release automation
    end
  end

  def install
    bin.install "recursive"
  end

  def caveats
    <<~EOS
      To get started:
        recursive           # open interactive TUI
        recursive -p 'goal' # one-shot agent run
        recursive --help    # all commands

      Set your API key:
        export ANTHROPIC_API_KEY=sk-ant-...
        # or: export OPENAI_API_KEY=sk-...

      Config file (optional):
        ~/.recursive/config.toml
    EOS
  end

  test do
    assert_match "recursive", shell_output("#{bin}/recursive --version")
  end
end
