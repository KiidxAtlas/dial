pub fn bounded_transaction_total(transactions: &[u64], ceiling: u64) -> u64 {
    transactions.iter().fold(0, |sum: u64, value| sum.saturating_add(*value).min(ceiling))
}

pub fn transaction_count(transactions: &[u64]) -> usize {
    transactions.len()
}
