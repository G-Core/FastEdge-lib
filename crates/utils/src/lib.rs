mod dictionary;

use std::sync::Arc;
pub use dictionary::Dictionary;

pub trait UserDiagStats: Sync + Send {
    /// Set user defined diagnostic information
    fn set_user_diag(&self, diag: &str);
}

pub struct Utils {
    stats: Arc<dyn UserDiagStats>
}

impl Utils {
    pub fn new(stats: Arc<dyn UserDiagStats>) -> Self {
        Self { stats }
    }
}

impl reactor::gcore::fastedge::utils::Host for Utils {
    async fn set_user_diag(&mut self, value: String)  {
        self.stats.set_user_diag(value.as_str());
    }
}
