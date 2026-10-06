# Claude Ship Terminal Focus

Companion extension for the [Claude Ship](https://github.com/python2121/claude-status) macOS menubar app. When you click a session row in the app, this extension selects the integrated terminal that Claude Code session is running in, across VS Code windows and across several sessions in one window.

It watches one request file under the app's Application Support directory, matches the request's process ids against this window's terminals, and answers with the workspace path so the app can raise the right window. It runs no commands and reads nothing else.

Installed automatically by the app's `install.sh`; remove with `code --uninstall-extension claude-status.claude-status-terminal-focus`.
