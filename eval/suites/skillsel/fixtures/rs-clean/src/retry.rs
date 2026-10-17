use std::thread;
use std::time::Duration;

/// Call `op` up to `attempts` times, sleeping `delay` between failures; the last error is returned.
pub fn retry<T, E>(attempts: u32, delay: Duration, mut op: impl FnMut(u32) -> Result<T, E>) -> Result<T, E> {
    let mut attempt = 1;
    loop {
        match op(attempt) {
            Ok(value) => return Ok(value),
            Err(error) if attempt >= attempts => return Err(error),
            Err(_) => {
                thread::sleep(delay);
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn succeeds_on_third_attempt() {
        let got = retry(3, Duration::ZERO, |n| if n < 3 { Err(n) } else { Ok(n) });
        assert_eq!(got, Ok(3));
    }

    #[test]
    fn returns_last_error() {
        let got: Result<(), u32> = retry(2, Duration::ZERO, Err);
        assert_eq!(got, Err(2));
    }
}
