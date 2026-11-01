"""A playable terminal snake game built on top of the movement fixture.

The pure-logic ``advance(body, direction)`` function is imported unchanged by
``test_snake.py`` and reused for every step here, so the two unit tests keep
passing while this module adds rendering, keyboard input, food, scoring and
game-over handling around it.

    python3 snake.py

Controls: arrow keys or WASD to steer (a 180-degree reversal is rejected). Press
``q`` or ``ESC`` to quit; after a run ends any key starts the next round. The
board sits below a status line and grows faster with every food eaten.
"""

import curses
import random

# Board geometry in cells: y grows downward, x grows rightward -- matching the
# (dy, dx) convention used by advance() and the fixture's tests.
BOARD_H = 20
BOARD_W = 40
MARGIN = 1  # one cell of border around the play area

# Base tick length in milliseconds; shrinks a little with every food eaten so
# the snake speeds up, clamped to stay playable.
INITIAL_INTERVAL_MS = 130
MIN_INTERVAL_MS = 55

UP = (-1, 0)
DOWN = (1, 0)
LEFT = (0, -1)
RIGHT = (0, 1)


def advance(body, direction):
    y, x = body[0]
    dy, dx = direction
    return [(y + dy, x + dx), *body[:-1]]


def opposite(a, b):
    """True when directions a and b point at each other."""
    return a == (-b[0], -b[1])


def clamp(value, low, high):
    return max(low, min(high, value))


def free_cell(body, height, width):
    """A random empty cell not occupied by the body (assumes one exists)."""
    taken = set(body)
    cells = [(y, x) for y in range(height) for x in range(width)]
    return next(c for c in random.sample(cells, len(cells) - len(taken)) if c not in taken)


def choose_direction(current, previous, key):
    """Map a pressed key to (dy, dx), rejecting 180-degree reversals."""
    want = None
    if key == curses.KEY_UP:
        want = UP
    elif key == curses.KEY_DOWN:
        want = DOWN
    elif key == curses.KEY_LEFT:
        want = LEFT
    elif key == curses.KEY_RIGHT:
        want = RIGHT
    else:
        if 0 <= key <= 255:
            ch = chr(key)
            letters = {'w': UP, 's': DOWN, 'a': LEFT, 'd': RIGHT}
            if ch in letters:
                want = letters[ch]
    return current if opposite(want, previous) else want


def reset():
    mid_y, mid_x = BOARD_H // 2, BOARD_W // 2
    snake = [(mid_y, mid_x - 2), (mid_y, mid_x - 1), (mid_y, mid_x)]
    return {
        'snake': snake,
        'direction': RIGHT,
        'previous': RIGHT,
        'interval': INITIAL_INTERVAL_MS,
        'score': 0,
        'best': 0,
        'food': None,
    }


def _put(stdscr, y, x, ch, attr, maxy, maxx):
    """Bounds-checked addstr; curses raises on the final cell of a row."""
    if y < 0 or y >= maxy or x < 0 or x >= maxx:
        return
    try:
        stdscr.addstr(y, x, ch, attr)
    except curses.error:
        pass


def setup_colors():
    """Install color pairs once the screen is initialized (called from main)."""
    if not curses.has_colors():
        return
    curses.start_color()
    try:
        curses.use_default_colors()
        curses.init_pair(1, curses.COLOR_WHITE, -1)   # head
        curses.init_pair(2, curses.COLOR_GREEN, -1)   # body
        curses.init_pair(3, curses.COLOR_RED, -1)     # food
        curses.init_pair(4, curses.COLOR_BLUE, -1)    # border / status
    except curses.error:
        pass


def draw_field(stdscr, state):
    maxy, maxx = stdscr.getmaxyx()
    stdscr.erase()

    top, left, right = MARGIN, MARGIN, BOARD_W + MARGIN - 1
    attrs = {
        'border': curses.color_pair(4),
        'head': curses.color_pair(1) | curses.A_BOLD,
        'body': curses.color_pair(2),
        'food': curses.color_pair(3),
    }

    for y in range(top, maxy):
        _put(stdscr, y, left - 1, '|', attrs['border'], maxy, maxx)
        _put(stdscr, y, right + 1, '|', attrs['border'], maxy, maxx)
    if left > 0:
        _put(stdscr, top - 1, left, '+', attrs['border'], maxy, maxx)
        _put(stdscr, top - 1, right, '+', attrs['border'], maxy, maxx)
        for x in range(left, right + 1):
            _put(stdscr, top - 1, x, '-', attrs['border'], maxy, maxx)
    if right < maxx - 1:
        for x in range(left, right + 1):
            _put(stdscr, maxy - 1, x, '-', attrs['border'], maxy, maxx)

    snake = state['snake']
    food = state['food']
    for i, seg in enumerate(snake):
        kind = 'head' if i == 0 else 'body'
        _put(stdscr, seg[0] + top, seg[1] + left, 'O', attrs[kind], maxy, maxx)
    if food is not None:
        _put(stdscr, food[0] + top, food[1] + left, '*', attrs['food'], maxy, maxx)

    status = "SNAKE   score: %d   best: %d   arrows/WASD to move  q/ESC quit" % (state['score'], state['best'])
    _put(stdscr, 0, 0, status[:maxx - 1], curses.color_pair(4) | curses.A_DIM, maxy, maxx)


def game_over_screen(stdscr, state):
    maxy, maxx = stdscr.getmaxyx()
    top, left, right = MARGIN, MARGIN, BOARD_W + MARGIN - 1
    _put(stdscr, top - 1, left, '+', curses.color_pair(4), maxy, maxx)
    _put(stdscr, top - 1, right, '+', curses.color_pair(4), maxy, maxx)
    for x in range(left, right + 1):
        _put(stdscr, top - 1, x, '-', curses.color_pair(4), maxy, maxx)
        _put(stdscr, maxy - 1, x, '-', curses.color_pair(4), maxy, maxx)

    _put(stdscr, 0, 0, 'GAME OVER', curses.color_pair(3) | curses.A_BOLD, maxy, maxx)
    status = "score: %d   best: %d" % (state['score'], state['best'])
    _put(stdscr, 1, 0, status[:maxx - 1], curses.color_pair(4) | curses.A_DIM, maxy, maxx)
    _put(stdscr, maxy // 2, 0, 'press any key to play again', curses.color_pair(4), maxy, maxx)


def run_game(stdscr):
    """Play one round. Returns (state, quit_now); False means the snake died."""
    state = reset()
    while True:
        stdscr.timeout(state['interval'])
        draw_field(stdscr, state)

        try:
            key = stdscr.getch()
        except curses._ResizeEvent:
            continue  # redraw with the new terminal size

        if key in (ord('q'), ord('Q'), curses.KEY_ESCAPE):
            return state, True

        state['direction'] = choose_direction(state['direction'], state['previous'], key)
        state['previous'] = state['direction']

        stepped = advance(state['snake'], state['direction'])
        ate_food = stepped[0] == state['food']

        if ate_food:
            state['score'] += 1
            state['interval'] = clamp(state['interval'] - 8, MIN_INTERVAL_MS)
            state['snake'] = [*stepped, state['snake'][-1]]  # keep the tail -> grow
        else:
            state['snake'] = stepped

        head = state['snake'][0]
        if not (0 <= head[0] < BOARD_H and 0 <= head[1] < BOARD_W) or head in state['snake'][1:]:
            return state, False


def main(stdscr):
    curses.curs_set(0)
    setup_colors()

    while True:
        state, quit_now = run_game(stdscr)
        if quit_now:
            break  # exit cleanly after a manual quit; show nothing extra
        game_over_screen(stdscr, state)
        stdscr.getch()  # any key starts the next round


if __name__ == "__main__":
    curses.wrapper(main)
