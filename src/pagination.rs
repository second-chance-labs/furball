use futures::stream::{self, Stream, StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::client::{Client, Request};
use crate::error::{KalshiError, Result};

/// Position in a paginated listing: `None` before the first page, `Some(None)` after the last.
type Position = Option<Option<String>>;

impl Client {
    /// Streams every item of a cursor-paginated list endpoint.
    ///
    /// `items_key` names the response array (`"markets"`, `"trades"`, ...). Pages of `limit` items are
    /// requested lazily until the response `cursor` is empty or `max_items` have been yielded; the cap
    /// ends the stream without an error.
    pub fn paginate<T>(
        &self,
        request: Request,
        items_key: &'static str,
        limit: u32,
        max_items: usize,
    ) -> impl Stream<Item = Result<T>> + use<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let client = self.clone();
        stream::try_unfold(None::<Option<String>>, move |position: Position| {
            let client = client.clone();
            let request = request.clone();
            async move {
                let cursor = match position {
                    None => None,
                    Some(None) => return Ok::<_, KalshiError>(None),
                    Some(Some(cursor)) => Some(cursor),
                };
                let page = request.query("limit", limit).query_opt("cursor", cursor);
                let (items, next) = client.fetch_page::<T>(&page, items_key).await?;
                Ok(Some((items, Some(next))))
            }
        })
        .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
        .try_flatten()
        .take(max_items)
    }

    async fn fetch_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        items_key: &str,
    ) -> Result<(Vec<T>, Option<String>)> {
        let body = self.send_text(request).await?;
        let decode = |source| KalshiError::Decode {
            body: body.clone(),
            source,
        };
        let page: Value = serde_json::from_str(&body).map_err(decode)?;
        let items = match page.get(items_key) {
            Some(items) => serde_json::from_value(items.clone()).map_err(decode)?,
            None => Vec::new(),
        };
        let next = page
            .get("cursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        Ok((items, next))
    }
}
