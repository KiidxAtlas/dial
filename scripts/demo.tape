# Records the README demo GIF: Dial fixes a failing test, shows the test run, then /undo restores the file.
#
#   brew install vhs        # also installs ttyd; needs ffmpeg
#   vhs scripts/demo.tape   # writes docs/images/demo.gif
#
# Uses your configured default model; add `--model ...` to the dial line to choose another.
# Model speed varies, so adjust the long Sleep if the answer is not finished on screen.

Output docs/images/demo.gif

Require dial
Require python3

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 20
Set Theme "Builtin Dark"
Set TypingSpeed 40ms

Hide
Type "export DEMO=$(mktemp -d) && cd $DEMO && git init -q"
Enter
Type `printf 'def add(a, b):\n    return a - b\n' > calc.py`
Enter
Type `printf 'import unittest\nfrom calc import add\n\nclass T(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\nunittest.main()\n' > test_calc.py`
Enter
Type "git add -A && git -c user.name=demo -c user.email=demo@example.com commit -qm init && clear"
Enter
Show

Type "python3 test_calc.py"
Enter
Sleep 2s

Type "dial"
Enter
Sleep 3s

Type "the test in test_calc.py fails, fix it"
Sleep 500ms
Enter
Sleep 90s

Type "/undo"
Sleep 500ms
Enter
Sleep 4s
