# Local task fixture

Fixed local documents and code for `local-tasks.toml`. Each benchmark case gets a disposable copy.
Sources are local files only: no web or network content is used.

`snake.py` and `test_snake.py` deliberately contain separate movement and test-harness defects.
They are a pure-logic regression fixture inspired by a snake-game session, not a playable game.
Reference repairs and independent acceptance checks live in `test_local_tasks.py` and the suite.
