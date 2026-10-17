use axum::routing::post;
use axum::{Json, Router};

mod pricing;

pub use pricing::total_price;

/// Something with a price.
pub trait Priced {
    fn price(&self) -> f64;
}

/// An order as stored.
pub struct Order {
    pub id: u64,
    pub items: Vec<String>,
}

impl Priced for Order {
    fn price(&self) -> f64 {
        total_price(&self.items, 0.0)
    }
}

pub fn router() -> Router {
    Router::new().route("/api/orders", post(create_order).get(list_orders))
}

async fn create_order(Json(items): Json<Vec<String>>) -> Json<u64> {
    let order = Order { id: 1, items };
    Json(pricing::total_price(&order.items, 0.1) as u64)
}

async fn list_orders() -> Json<Vec<u64>> {
    Json(vec![])
}
