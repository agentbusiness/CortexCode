import time


def retry(fn, attempts=3, delay=0.5, exceptions=(OSError,)):
    """Call fn() until it succeeds, at most `attempts` times, sleeping `delay` seconds (doubling)
    between failures. The last exception is re-raised."""
    wait = delay
    for attempt in range(1, attempts + 1):
        try:
            return fn()
        except exceptions:
            if attempt == attempts:
                raise
            time.sleep(wait)
            wait *= 2
