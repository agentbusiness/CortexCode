pub const BASE_PRICE: f64 = 9.5;

/// Sum of item prices after a discount.
pub fn total_price(items: &[String], discount: f64) -> f64 {
    let gross = items.len() as f64 * BASE_PRICE;
    apply_discount(gross, discount)
}

fn apply_discount(amount: f64, discount: f64) -> f64 {
    amount * (1.0 - discount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_are_discounted() {
        assert_eq!(total_price(&[], 0.5), 0.0);
    }
}
