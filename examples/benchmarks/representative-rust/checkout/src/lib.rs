pub fn receipt(price: i64, quantity: i64) -> Result<String, &'static str> {
    Ok(format!("{} cents", pricing::line_total(price, quantity)?))
}
