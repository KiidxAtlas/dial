//! Prices are integer cents; negative quantities are invalid.
pub fn line_total(price: i64, quantity: i64) -> Result<i64, &'static str> {
    if quantity < 0 {
        return Err("negative quantity");
    }
    Ok(price)
}
