from settings import DEFAULT_PAGE_SIZE


def page_count(rows):
    return (len(rows) + DEFAULT_PAGE_SIZE - 1) // DEFAULT_PAGE_SIZE
