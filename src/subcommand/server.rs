use super::*;

use {
  self::deserialize_from_str::DeserializeFromStr,
  crate::templates::{
    BlockHtml, ClockSvg, HomeHtml, InputHtml, InscriptionHtml, InscriptionsHtml, OutputHtml,
    PageContent, PageHtml, RangeHtml, RareTxt, SatHtml, TransactionHtml,
  },
  axum::{
    body,
    extract::{Extension, Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
  },
  axum_server::Handle,
  rust_embed::RustEmbed,
  rustls_acme::{
    acme::{LETS_ENCRYPT_PRODUCTION_DIRECTORY, LETS_ENCRYPT_STAGING_DIRECTORY},
    axum::AxumAcceptor,
    caches::DirCache,
    AcmeConfig,
  },
  serde::{de, Deserializer},
  std::{cmp::Ordering, str},
  tokio_stream::StreamExt,
};

mod deserialize_from_str;

enum BlockQuery {
  Height(u64),
  Hash(BlockHash),
}

impl FromStr for BlockQuery {
  type Err = Error;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(if s.len() == 64 {
      BlockQuery::Hash(s.parse()?)
    } else {
      BlockQuery::Height(s.parse()?)
    })
  }
}

enum ServerError {
  Internal(Error),
  NotFound(String),
  BadRequest(String),
}

type ServerResult<T> = Result<T, ServerError>;

impl IntoResponse for ServerError {
  fn into_response(self) -> Response {
    match self {
      Self::Internal(error) => {
        eprintln!("error serving request: {error}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          StatusCode::INTERNAL_SERVER_ERROR
            .canonical_reason()
            .unwrap_or_default(),
        )
          .into_response()
      }
      Self::NotFound(message) => (StatusCode::NOT_FOUND, message).into_response(),
      Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
  }
}

#[derive(Deserialize)]
struct Search {
  query: String,
}

#[derive(RustEmbed)]
#[folder = "static"]
struct StaticAssets;

struct StaticHtml {
  title: &'static str,
  html: &'static str,
}

impl PageContent for StaticHtml {
  fn title(&self) -> String {
    self.title.into()
  }
}

impl Display for StaticHtml {
  fn fmt(&self, f: &mut Formatter) -> fmt::Result {
    f.write_str(self.html)
  }
}

#[derive(Debug, Parser)]
pub(crate) struct Server {
  #[clap(
    long,
    default_value = "0.0.0.0",
    help = "Listen on <ADDRESS> for incoming requests."
  )]
  address: String,
  #[clap(
    long,
    help = "Request ACME TLS certificate for <ACME_DOMAIN>. This ord instance must be reachable at <ACME_DOMAIN>:443 to respond to Let's Encrypt ACME challenges."
  )]
  acme_domain: Vec<String>,
  #[clap(
    long,
    help = "Listen on <HTTP_PORT> for incoming HTTP requests. [default: 80]."
  )]
  http_port: Option<u16>,
  #[clap(
    long,
    group = "port",
    help = "Listen on <HTTPS_PORT> for incoming HTTPS requests. [default: 443]."
  )]
  https_port: Option<u16>,
  #[clap(long, help = "Store ACME TLS certificates in <ACME_CACHE>.")]
  acme_cache: Option<PathBuf>,
  #[clap(long, help = "Provide ACME contact <ACME_CONTACT>.")]
  acme_contact: Vec<String>,
  #[clap(long, help = "Serve HTTP traffic on <HTTP_PORT>.")]
  http: bool,
  #[clap(long, help = "Serve HTTPS traffic on <HTTPS_PORT>.")]
  https: bool,
}

impl Server {
  pub(crate) fn run(self, options: Options, index: Arc<Index>, handle: Handle) -> Result {
    Runtime::new()?.block_on(async {
      let clone = index.clone();
      thread::spawn(move || loop {
        if let Err(error) = clone.update() {
          log::error!("{error}");
        }
        thread::sleep(Duration::from_millis(100));
      });

      let router = Router::new()
        .route("/", get(Self::home))
        .route("/block-count", get(Self::block_count))
        .route("/block/:query", get(Self::block))
        .route("/bounties", get(Self::bounties))
        .route("/clock", get(Self::clock))
        .route("/content/:inscription_id", get(Self::content))
        .route("/faq", get(Self::faq))
        .route("/favicon.ico", get(Self::favicon))
        .route("/input/:block/:transaction/:input", get(Self::input))
        .route("/inscription/:inscription_id", get(Self::inscription))
        .route("/inscriptions", get(Self::inscriptions))
        .route("/install.sh", get(Self::install_script))
        .route("/ordinal/:sat", get(Self::ordinal))
        .route("/output/:output", get(Self::output))
        .route("/range/:start/:end", get(Self::range))
        .route("/rare.txt", get(Self::rare_txt))
        .route("/sat/:sat", get(Self::sat))
        .route("/search", get(Self::search_by_query))
        .route("/search/:query", get(Self::search_by_path))
        .route("/static/*path", get(Self::static_asset))
        .route("/status", get(Self::status))
        .route("/tx/:txid", get(Self::transaction))
        .layer(Extension(index))
        .layer(Extension(options.chain()))
        .layer(
          CorsLayer::new()
            .allow_methods([http::Method::GET])
            .allow_origin(Any),
        );

      match (self.http_port(), self.https_port()) {
        (Some(http_port), None) => self.spawn(router, handle, http_port, None)?.await??,
        (None, Some(https_port)) => {
          self
            .spawn(router, handle, https_port, Some(self.acceptor(&options)?))?
            .await??
        }
        (Some(http_port), Some(https_port)) => {
          let (http_result, https_result) = tokio::join!(
            self.spawn(router.clone(), handle.clone(), http_port, None)?,
            self.spawn(router, handle, https_port, Some(self.acceptor(&options)?))?
          );
          http_result.and(https_result)??;
        }
        (None, None) => unreachable!(),
      }

      Ok(())
    })
  }

  fn spawn(
    &self,
    router: Router,
    handle: Handle,
    port: u16,
    https_acceptor: Option<AxumAcceptor>,
  ) -> Result<task::JoinHandle<io::Result<()>>> {
    let addr = (self.address.as_str(), port)
      .to_socket_addrs()?
      .next()
      .ok_or_else(|| anyhow!("failed to get socket addrs"))?;

    if !integration_test() {
      eprintln!(
        "Listening on {}://{addr}",
        if https_acceptor.is_some() {
          "https"
        } else {
          "http"
        }
      );
    }

    Ok(tokio::spawn(async move {
      if let Some(acceptor) = https_acceptor {
        axum_server::Server::bind(addr)
          .handle(handle)
          .acceptor(acceptor)
          .serve(router.into_make_service())
          .await
      } else {
        axum_server::Server::bind(addr)
          .handle(handle)
          .serve(router.into_make_service())
          .await
      }
    }))
  }

  fn acme_cache(acme_cache: Option<&PathBuf>, options: &Options) -> Result<PathBuf> {
    let acme_cache = if let Some(acme_cache) = acme_cache {
      acme_cache.clone()
    } else {
      options.data_dir()?.join("acme-cache")
    };

    Ok(acme_cache)
  }

  fn acme_domains(acme_domain: &Vec<String>) -> Result<Vec<String>> {
    if !acme_domain.is_empty() {
      Ok(acme_domain.clone())
    } else {
      Ok(vec![sys_info::hostname()?])
    }
  }

  fn http_port(&self) -> Option<u16> {
    if self.http || self.http_port.is_some() || (self.https_port.is_none() && !self.https) {
      Some(self.http_port.unwrap_or(80))
    } else {
      None
    }
  }

  fn https_port(&self) -> Option<u16> {
    if self.https || self.https_port.is_some() {
      Some(self.https_port.unwrap_or(443))
    } else {
      None
    }
  }

  fn acceptor(&self, options: &Options) -> Result<AxumAcceptor> {
    let config = AcmeConfig::new(Self::acme_domains(&self.acme_domain)?)
      .contact(&self.acme_contact)
      .cache_option(Some(DirCache::new(Self::acme_cache(
        self.acme_cache.as_ref(),
        options,
      )?)))
      .directory(if cfg!(test) {
        LETS_ENCRYPT_STAGING_DIRECTORY
      } else {
        LETS_ENCRYPT_PRODUCTION_DIRECTORY
      });

    let mut state = config.state();

    let acceptor = state.axum_acceptor(Arc::new(
      rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_cert_resolver(state.resolver()),
    ));

    tokio::spawn(async move {
      while let Some(result) = state.next().await {
        match result {
          Ok(ok) => log::info!("ACME event: {:?}", ok),
          Err(err) => log::error!("ACME error: {:?}", err),
        }
      }
    });

    Ok(acceptor)
  }

  fn index_height(index: &Index) -> ServerResult<Height> {
    index
      .height()
      .map_err(|err| ServerError::Internal(anyhow!("failed to retrieve height from index: {err}")))?
      .ok_or_else(|| ServerError::Internal(anyhow!("index has not indexed genesis block")))
  }

  async fn clock(Extension(index): Extension<Arc<Index>>) -> ServerResult<ClockSvg> {
    Ok(ClockSvg::new(Self::index_height(&index)?))
  }

  async fn sat(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
    Path(DeserializeFromStr(sat)): Path<DeserializeFromStr<Sat>>,
  ) -> ServerResult<PageHtml> {
    let satpoint = index.rare_sat_satpoint(sat).map_err(|err| {
      ServerError::Internal(anyhow!(
        "failed to satpoint for sat {sat} from index: {err}"
      ))
    })?;

    Ok(
      SatHtml {
        sat,
        satpoint,
        blocktime: index.blocktime(sat.height()).map_err(|err| {
          ServerError::Internal(anyhow!("failed to retrieve blocktime from index: {err}"))
        })?,
        inscription: index.get_inscription_by_sat(sat).map_err(|err| {
          ServerError::Internal(anyhow!(
            "failed to retrieve inscription for sat {sat} from index: {err}"
          ))
        })?,
      }
      .page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }

  async fn ordinal(Path(sat): Path<String>) -> Redirect {
    Redirect::to(&format!("/sat/{sat}"))
  }

  async fn output(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
    Path(outpoint): Path<OutPoint>,
  ) -> ServerResult<PageHtml> {
    let output = index
      .get_transaction(outpoint.txid)
      .map_err(ServerError::Internal)?
      .ok_or_else(|| ServerError::NotFound(format!("output {outpoint} unknown")))?
      .output
      .into_iter()
      .nth(outpoint.vout as usize)
      .ok_or_else(|| ServerError::NotFound(format!("output {outpoint} unknown")))?;

    Ok(
      OutputHtml {
        outpoint,
        list: if index.has_satoshi_index().map_err(ServerError::Internal)? {
          Some(
            index
              .list(outpoint)
              .map_err(ServerError::Internal)?
              .ok_or_else(|| ServerError::NotFound(format!("output {outpoint} unknown")))?,
          )
        } else {
          None
        },
        chain,
        output,
      }
      .page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }

  async fn range(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
    Path((DeserializeFromStr(start), DeserializeFromStr(end))): Path<(
      DeserializeFromStr<Sat>,
      DeserializeFromStr<Sat>,
    )>,
  ) -> ServerResult<PageHtml> {
    match start.cmp(&end) {
      Ordering::Equal => Err(ServerError::BadRequest("empty range".to_string())),
      Ordering::Greater => Err(ServerError::BadRequest(
        "range start greater than range end".to_string(),
      )),
      Ordering::Less => Ok(RangeHtml { start, end }.page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      )),
    }
  }

  async fn rare_txt(Extension(index): Extension<Arc<Index>>) -> ServerResult<RareTxt> {
    Ok(RareTxt(
      index
        .rare_sat_satpoints()
        .map_err(|err| ServerError::Internal(anyhow!("error getting rare sat satpoints: {err}")))?
        .ok_or_else(|| {
          ServerError::NotFound(
            "tracking rare sats requires index created with `--index-sats` flag".into(),
          )
        })?,
    ))
  }

  async fn home(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
  ) -> ServerResult<PageHtml> {
    Ok(
      HomeHtml::new(
        index
          .blocks(100)
          .map_err(|err| ServerError::Internal(anyhow!("error getting blocks: {err}")))?,
        index
          .get_latest_inscriptions(8)
          .map_err(|err| ServerError::Internal(anyhow!("error getting inscriptions: {err}")))?,
      )
      .page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }

  async fn install_script() -> Redirect {
    Redirect::to("https://raw.githubusercontent.com/casey/ord/master/install.sh")
  }

  async fn block(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
    Path(DeserializeFromStr(query)): Path<DeserializeFromStr<BlockQuery>>,
  ) -> ServerResult<PageHtml> {
    let (block, height) = match query {
      BlockQuery::Height(height) => {
        let block = index
          .get_block_by_height(height)
          .map_err(|err| {
            ServerError::Internal(anyhow!(
              "error serving request for block with height {height}: {err}"
            ))
          })?
          .ok_or_else(|| ServerError::NotFound(format!("block at height {height} unknown")))?;

        (block, height)
      }
      BlockQuery::Hash(hash) => {
        let info = index
          .block_header_info(hash)
          .map_err(|err| {
            ServerError::Internal(anyhow!(
              "error serving request for block with hash {hash}: {err}"
            ))
          })?
          .ok_or_else(|| ServerError::NotFound(format!("block {hash} unknown")))?;

        let block = index
          .get_block_by_hash(hash)
          .map_err(|err| {
            ServerError::Internal(anyhow!(
              "error serving request for block with hash {hash}: {err}"
            ))
          })?
          .ok_or_else(|| ServerError::NotFound(format!("block {hash} unknown")))?;

        (block, info.height as u64)
      }
    };

    Ok(
      BlockHtml::new(block, Height(height), Self::index_height(&index)?).page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }

  async fn transaction(
    Extension(index): Extension<Arc<Index>>,
    Extension(chain): Extension<Chain>,
    Path(txid): Path<Txid>,
  ) -> ServerResult<PageHtml> {
    let inscription = index
      .get_inscription_by_inscription_id(txid)
      .map_err(|err| {
        ServerError::Internal(anyhow!(
          "failed to retrieve inscription from txid {txid} from index: {err}"
        ))
      })?
      .map(|(inscription, _satpoint)| inscription);

    Ok(
      TransactionHtml::new(
        index
          .get_transaction(txid)
          .map_err(|err| {
            ServerError::Internal(anyhow!(
              "error serving request for transaction {txid}: {err}"
            ))
          })?
          .ok_or_else(|| ServerError::NotFound(format!("transaction {txid} unknown")))?,
        inscription,
        chain,
      )
      .page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }

  async fn status(Extension(index): Extension<Arc<Index>>) -> (StatusCode, &'static str) {
    if index.is_reorged() {
      (
        StatusCode::OK,
        "reorg detected, please rebuild the database.",
      )
    } else {
      (
        StatusCode::OK,
        StatusCode::OK.canonical_reason().unwrap_or_default(),
      )
    }
  }

  async fn search_by_query(
    Extension(index): Extension<Arc<Index>>,
    Query(search): Query<Search>,
  ) -> ServerResult<Redirect> {
    Self::search(&index, &search.query).await
  }

  async fn search_by_path(
    Extension(index): Extension<Arc<Index>>,
    Path(search): Path<Search>,
  ) -> ServerResult<Redirect> {
    Self::search(&index, &search.query).await
  }

  async fn search(index: &Index, query: &str) -> ServerResult<Redirect> {
    Self::search_inner(index, query)
  }

  fn search_inner(index: &Index, query: &str) -> ServerResult<Redirect> {
    lazy_static! {
      static ref HASH: Regex = Regex::new(r"^[[:xdigit:]]{64}$").unwrap();
      static ref OUTPOINT: Regex = Regex::new(r"^[[:xdigit:]]{64}:\d+$").unwrap();
    }

    let query = query.trim();

    if HASH.is_match(query) {
      if index
        .block_header(query.parse().unwrap())
        .map_err(|err| {
          ServerError::Internal(anyhow!(
            "failed to retrieve block {query} from index: {err}"
          ))
        })?
        .is_some()
      {
        Ok(Redirect::to(&format!("/block/{query}")))
      } else {
        Ok(Redirect::to(&format!("/tx/{query}")))
      }
    } else if OUTPOINT.is_match(query) {
      Ok(Redirect::to(&format!("/output/{query}")))
    } else {
      Ok(Redirect::to(&format!("/sat/{query}")))
    }
  }

  async fn favicon() -> ServerResult<Response> {
    Self::static_asset(Path("/favicon.png".to_string())).await
  }

  async fn static_asset(Path(path): Path<String>) -> ServerResult<Response> {
    let content = StaticAssets::get(if let Some(stripped) = path.strip_prefix('/') {
      stripped
    } else {
      &path
    })
    .ok_or_else(|| ServerError::NotFound(format!("asset {path} unknown")))?;
    let body = body::boxed(body::Full::from(content.data));
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Ok(
      Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(body)
        .unwrap(),
    )
  }

  async fn block_count(Extension(index): Extension<Arc<Index>>) -> ServerResult<String> {
    Ok(
      index
        .block_count()
        .map_err(|err| {
          ServerError::Internal(anyhow!("failed to retrieve block count from index: {err}"))
        })?
        .to_string(),
    )
  }

  async fn input(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
    Path(path): Path<(u64, usize, usize)>,
  ) -> Result<PageHtml, ServerError> {
    let not_found =
      || ServerError::NotFound(format!("input /{}/{}/{} unknown", path.0, path.1, path.2));

    let block = index
      .get_block_by_height(path.0)
      .map_err(ServerError::Internal)?
      .ok_or_else(not_found)?;

    let transaction = block.txdata.into_iter().nth(path.1).ok_or_else(not_found)?;

    let input = transaction
      .input
      .into_iter()
      .nth(path.2)
      .ok_or_else(not_found)?;

    Ok(InputHtml { path, input }.page(
      chain,
      index.has_satoshi_index().map_err(ServerError::Internal)?,
    ))
  }

  async fn faq() -> Redirect {
    Redirect::to("https://docs.ordinals.com/faq/")
  }

  async fn bounties() -> Redirect {
    Redirect::to("https://docs.ordinals.com/bounty/")
  }

  async fn content(
    Extension(index): Extension<Arc<Index>>,
    Path(inscription_id): Path<InscriptionId>,
  ) -> ServerResult<Response> {
    let (inscription, _) = index
      .get_inscription_by_inscription_id(inscription_id)
      .map_err(|err| {
        ServerError::Internal(anyhow!(
          "failed to retrieve inscription with inscription id {inscription_id} from index: {err}"
        ))
      })?
      .ok_or_else(|| {
        ServerError::NotFound(format!("transaction {inscription_id} has no inscription"))
      })?;

    let (content_type, content) = Self::content_response(inscription).ok_or_else(|| {
      ServerError::NotFound(format!("inscription {inscription_id} has no content"))
    })?;

    Ok(
      (
        [
          (header::CONTENT_TYPE, content_type),
          (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none' 'unsafe-eval' 'unsafe-inline'".to_string(),
          ),
        ],
        content,
      )
        .into_response(),
    )
  }

  fn content_response(inscription: Inscription) -> Option<(String, Vec<u8>)> {
    let content = inscription.content_bytes()?;

    match inscription.content_type() {
      Some(content_type) => Some((content_type.into(), content.to_vec())),
      None => Some(("application/octet-stream".into(), content.to_vec())),
    }
  }

  async fn inscription(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
    Path(inscription_id): Path<InscriptionId>,
  ) -> ServerResult<PageHtml> {
    let (inscription, satpoint) = index
      .get_inscription_by_inscription_id(inscription_id)
      .map_err(|err| {
        ServerError::Internal(anyhow!(
          "failed to retrieve inscription with inscription id {inscription_id} from index: {err}"
        ))
      })?
      .ok_or_else(|| {
        ServerError::NotFound(format!("transaction {inscription_id} has no inscription"))
      })?;

    let genesis_height = index.get_genesis_height(inscription_id).map_err(|err| {
        ServerError::Internal(anyhow!(
          "failed to retrieve height for inscriptiom with inscription id {inscription_id} from index: {err}"
        ))
      })?;

    Ok(
      InscriptionHtml {
        genesis_height,
        inscription_id,
        inscription,
        satpoint,
      }
      .page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }

  async fn inscriptions(
    Extension(chain): Extension<Chain>,
    Extension(index): Extension<Arc<Index>>,
  ) -> ServerResult<PageHtml> {
    Ok(
      InscriptionsHtml {
        inscriptions: index
          .get_latest_inscriptions(100)
          .map_err(|err| ServerError::Internal(anyhow!("error getting inscriptions: {err}")))?,
      }
      .page(
        chain,
        index.has_satoshi_index().map_err(ServerError::Internal)?,
      ),
    )
  }
}

#[cfg(test)]
mod tests {
  use {super::*, reqwest::Url, std::net::TcpListener, tempfile::TempDir};

  struct TestServer {
    bitcoin_rpc_server: test_bitcoincore_rpc::Handle,
    index: Arc<Index>,
    ord_server_handle: Handle,
    url: Url,
    #[allow(unused)]
    tempdir: TempDir,
  }

  impl TestServer {
    fn new() -> Self {
      Self::new_with_args(&[])
    }

    fn new_with_args(args: &[&str]) -> Self {
      let bitcoin_rpc_server = test_bitcoincore_rpc::spawn();

      let tempdir = TempDir::new().unwrap();

      let cookiefile = tempdir.path().join("cookie");

      fs::write(&cookiefile, "username:password").unwrap();

      let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

      let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();

      let (options, server) = parse_server_args(&format!(
        "ord --chain regtest --rpc-url {} --cookie-file {} --data-dir {} {} server --http-port {} --address 127.0.0.1",
        bitcoin_rpc_server.url(),
        cookiefile.to_str().unwrap(),
        tempdir.path().to_str().unwrap(),
        args.join(" "),
        port,
      ));

      let index = Arc::new(Index::open(&options).unwrap());
      let ord_server_handle = Handle::new();

      {
        let index = index.clone();
        let ord_server_handle = ord_server_handle.clone();
        thread::spawn(|| server.run(options, index, ord_server_handle).unwrap());
      }

      while index.statistic(crate::index::Statistic::Commits).unwrap() == 0 {
        thread::sleep(Duration::from_millis(25));
      }

      for i in 0.. {
        match reqwest::blocking::get(format!("http://127.0.0.1:{port}/status")) {
          Ok(_) => break,
          Err(err) => {
            if i == 400 {
              panic!("server failed to start: {err}");
            }
          }
        }

        thread::sleep(Duration::from_millis(25));
      }

      Self {
        bitcoin_rpc_server,
        index,
        ord_server_handle,
        tempdir,
        url,
      }
    }

    fn get(&self, path: &str) -> reqwest::blocking::Response {
      if let Err(error) = self.index.update() {
        log::error!("{error}");
      }
      reqwest::blocking::get(self.join_url(path)).unwrap()
    }

    fn join_url(&self, url: &str) -> Url {
      self.url.join(url).unwrap()
    }

    fn assert_response(&self, path: &str, status: StatusCode, expected_response: &str) {
      let response = self.get(path);
      assert_eq!(response.status(), status, "{}", response.text().unwrap());
      pretty_assert_eq!(response.text().unwrap(), expected_response);
    }

    fn assert_response_regex(&self, path: &str, status: StatusCode, regex: &str) {
      let response = self.get(path);
      assert_eq!(response.status(), status);
      assert_regex_match!(response.text().unwrap(), regex);
    }

    fn assert_redirect(&self, path: &str, location: &str) {
      let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(self.join_url(path))
        .send()
        .unwrap();

      assert_eq!(response.status(), StatusCode::SEE_OTHER);
      assert_eq!(response.headers().get(header::LOCATION).unwrap(), location);
    }
  }

  impl Drop for TestServer {
    fn drop(&mut self) {
      self.ord_server_handle.shutdown();
    }
  }

  fn parse_server_args(args: &str) -> (Options, Server) {
    match Arguments::try_parse_from(args.split_whitespace()) {
      Ok(arguments) => match arguments.subcommand {
        Subcommand::Server(server) => (arguments.options, server),
        subcommand => panic!("unexpected subcommand: {subcommand:?}"),
      },
      Err(err) => panic!("error parsing arguments: {err}"),
    }
  }

  #[test]
  fn http_and_https_port_dont_conflict() {
    parse_server_args(
      "ord server --http-port 0 --https-port 0 --acme-cache foo --acme-contact bar --acme-domain baz",
    );
  }

  #[test]
  fn http_port_defaults_to_80() {
    assert_eq!(parse_server_args("ord server").1.http_port(), Some(80));
  }

  #[test]
  fn https_port_defaults_to_none() {
    assert_eq!(parse_server_args("ord server").1.https_port(), None);
  }

  #[test]
  fn https_sets_https_port_to_443() {
    assert_eq!(
      parse_server_args("ord server --https --acme-cache foo --acme-contact bar --acme-domain baz")
        .1
        .https_port(),
      Some(443)
    );
  }

  #[test]
  fn https_disables_http() {
    assert_eq!(
      parse_server_args("ord server --https --acme-cache foo --acme-contact bar --acme-domain baz")
        .1
        .http_port(),
      None
    );
  }

  #[test]
  fn https_port_disables_http() {
    assert_eq!(
      parse_server_args(
        "ord server --https-port 433 --acme-cache foo --acme-contact bar --acme-domain baz"
      )
      .1
      .http_port(),
      None
    );
  }

  #[test]
  fn https_port_sets_https_port() {
    assert_eq!(
      parse_server_args(
        "ord server --https-port 1000 --acme-cache foo --acme-contact bar --acme-domain baz"
      )
      .1
      .https_port(),
      Some(1000)
    );
  }

  #[test]
  fn http_with_https_leaves_http_enabled() {
    assert_eq!(
      parse_server_args(
        "ord server --https --http --acme-cache foo --acme-contact bar --acme-domain baz"
      )
      .1
      .http_port(),
      Some(80)
    );
  }

  #[test]
  fn http_with_https_leaves_https_enabled() {
    assert_eq!(
      parse_server_args(
        "ord server --https --http --acme-cache foo --acme-contact bar --acme-domain baz"
      )
      .1
      .https_port(),
      Some(443)
    );
  }

  #[test]
  fn acme_contact_accepts_multiple_values() {
    assert!(Arguments::try_parse_from([
      "ord",
      "server",
      "--address",
      "127.0.0.1",
      "--http-port",
      "0",
      "--acme-contact",
      "foo",
      "--acme-contact",
      "bar"
    ])
    .is_ok());
  }

  #[test]
  fn acme_domain_accepts_multiple_values() {
    assert!(Arguments::try_parse_from([
      "ord",
      "server",
      "--address",
      "127.0.0.1",
      "--http-port",
      "0",
      "--acme-domain",
      "foo",
      "--acme-domain",
      "bar"
    ])
    .is_ok());
  }

  #[test]
  fn acme_cache_defaults_to_data_dir() {
    let arguments = Arguments::try_parse_from(["ord", "--data-dir", "foo", "server"]).unwrap();
    let acme_cache = Server::acme_cache(None, &arguments.options)
      .unwrap()
      .display()
      .to_string();
    assert!(
      acme_cache.contains(if cfg!(windows) {
        r"foo\acme-cache"
      } else {
        "foo/acme-cache"
      }),
      "{acme_cache}"
    )
  }

  #[test]
  fn acme_cache_flag_is_respected() {
    let arguments =
      Arguments::try_parse_from(["ord", "--data-dir", "foo", "server", "--acme-cache", "bar"])
        .unwrap();
    let acme_cache = Server::acme_cache(Some(&"bar".into()), &arguments.options)
      .unwrap()
      .display()
      .to_string();
    assert_eq!(acme_cache, "bar")
  }

  #[test]
  fn acme_domain_defaults_to_hostname() {
    assert_eq!(
      Server::acme_domains(&Vec::new()).unwrap(),
      &[sys_info::hostname().unwrap()]
    );
  }

  #[test]
  fn acme_domain_flag_is_respected() {
    assert_eq!(
      Server::acme_domains(&vec!["example.com".into()]).unwrap(),
      &["example.com"]
    );
  }

  #[test]
  fn install_sh_redirects_to_github() {
    TestServer::new().assert_redirect(
      "/install.sh",
      "https://raw.githubusercontent.com/casey/ord/master/install.sh",
    );
  }

  #[test]
  fn ordinal_redirects_to_sat() {
    TestServer::new().assert_redirect("/ordinal/0", "/sat/0");
  }

  #[test]
  fn bounties_redirects_to_docs_site() {
    TestServer::new().assert_redirect("/bounties", "https://docs.ordinals.com/bounty/");
  }

  #[test]
  fn faq_redirects_to_docs_site() {
    TestServer::new().assert_redirect("/faq", "https://docs.ordinals.com/faq/");
  }

  #[test]
  fn search_by_query_returns_sat() {
    TestServer::new().assert_redirect("/search?query=0", "/sat/0");
  }

  #[test]
  fn search_is_whitespace_insensitive() {
    TestServer::new().assert_redirect("/search/ 0 ", "/sat/0");
  }

  #[test]
  fn search_by_path_returns_sat() {
    TestServer::new().assert_redirect("/search/0", "/sat/0");
  }

  #[test]
  fn search_for_blockhash_returns_block() {
    TestServer::new().assert_redirect(
      "/search/000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
      "/block/000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
    );
  }

  #[test]
  fn search_for_txid_returns_transaction() {
    TestServer::new().assert_redirect(
      "/search/0000000000000000000000000000000000000000000000000000000000000000",
      "/tx/0000000000000000000000000000000000000000000000000000000000000000",
    );
  }

  #[test]
  fn search_for_outpoint_returns_output() {
    TestServer::new().assert_redirect(
      "/search/0000000000000000000000000000000000000000000000000000000000000000:0",
      "/output/0000000000000000000000000000000000000000000000000000000000000000:0",
    );
  }

  #[test]
  fn status() {
    TestServer::new().assert_response("/status", StatusCode::OK, "OK");
  }

  #[test]
  fn block_count_endpoint() {
    let test_server = TestServer::new();

    let response = test_server.get("/block-count");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().unwrap(), "1");

    test_server.bitcoin_rpc_server.mine_blocks(1);

    let response = test_server.get("/block-count");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().unwrap(), "2");
  }

  #[test]
  fn range_end_before_range_start_returns_400() {
    TestServer::new().assert_response(
      "/range/1/0",
      StatusCode::BAD_REQUEST,
      "range start greater than range end",
    );
  }

  #[test]
  fn invalid_range_start_returns_400() {
    TestServer::new().assert_response(
      "/range/=/0",
      StatusCode::BAD_REQUEST,
      "Invalid URL: invalid digit found in string",
    );
  }

  #[test]
  fn invalid_range_end_returns_400() {
    TestServer::new().assert_response(
      "/range/0/=",
      StatusCode::BAD_REQUEST,
      "Invalid URL: invalid digit found in string",
    );
  }

  #[test]
  fn empty_range_returns_400() {
    TestServer::new().assert_response("/range/0/0", StatusCode::BAD_REQUEST, "empty range");
  }

  #[test]
  fn range() {
    TestServer::new().assert_response_regex(
      "/range/0/1",
      StatusCode::OK,
      r".*<title>Sat range 0–1</title>.*<h1>Sat range 0–1</h1>
<dl>
  <dt>value</dt><dd>1</dd>
  <dt>first</dt><dd><a href=/sat/0 class=mythic>0</a></dd>
</dl>.*",
    );
  }
  #[test]
  fn sat_number() {
    TestServer::new().assert_response_regex("/sat/0", StatusCode::OK, ".*<h1>Sat 0</h1>.*");
  }

  #[test]
  fn sat_decimal() {
    TestServer::new().assert_response_regex("/sat/0.0", StatusCode::OK, ".*<h1>Sat 0</h1>.*");
  }

  #[test]
  fn sat_degree() {
    TestServer::new().assert_response_regex("/sat/0°0′0″0‴", StatusCode::OK, ".*<h1>Sat 0</h1>.*");
  }

  #[test]
  fn sat_name() {
    TestServer::new().assert_response_regex(
      "/sat/nvtdijuwxlp",
      StatusCode::OK,
      ".*<h1>Sat 0</h1>.*",
    );
  }

  #[test]
  fn sat() {
    TestServer::new().assert_response_regex(
      "/sat/0",
      StatusCode::OK,
      ".*<title>0°0′0″0‴</title>.*<h1>Sat 0</h1>.*",
    );
  }

  #[test]
  fn sat_out_of_range() {
    TestServer::new().assert_response(
      "/sat/2099999997690000",
      StatusCode::BAD_REQUEST,
      "Invalid URL: invalid sat",
    );
  }

  #[test]
  fn invalid_outpoint_hash_returns_400() {
    TestServer::new().assert_response(
      "/output/foo:0",
      StatusCode::BAD_REQUEST,
      "Invalid URL: error parsing TXID",
    );
  }

  #[test]
  fn output_with_satoshi_index() {
    TestServer::new_with_args(&["--index-sats"]).assert_response_regex(
    "/output/4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0",
    StatusCode::OK,
    ".*<title>Output 4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0</title>.*<h1>Output <span class=monospace>4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0</span></h1>
<dl>
  <dt>value</dt><dd>5000000000</dd>
  <dt>script pubkey</dt><dd class=data>OP_PUSHBYTES_65 04678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5f OP_CHECKSIG</dd>
</dl>
<h2>1 Sat Range</h2>
<ul class=monospace>
  <li><a href=/range/0/5000000000 class=mythic>0–5000000000</a></li>
</ul>.*",
  );
  }

  #[test]
  fn output_without_satoshi_index() {
    TestServer::new().assert_response_regex(
    "/output/4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0",
    StatusCode::OK,
    ".*<title>Output 4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0</title>.*<h1>Output <span class=monospace>4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0</span></h1>
<dl>
  <dt>value</dt><dd>5000000000</dd>
  <dt>script pubkey</dt><dd class=data>OP_PUSHBYTES_65 04678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5f OP_CHECKSIG</dd>
</dl>

  </main>
  </body>
</html>
",
  );
  }

  #[test]
  fn unknown_output_returns_404() {
    TestServer::new().assert_response(
      "/output/0000000000000000000000000000000000000000000000000000000000000000:0",
      StatusCode::NOT_FOUND,
      "output 0000000000000000000000000000000000000000000000000000000000000000:0 unknown",
    );
  }

  #[test]
  fn invalid_output_returns_400() {
    TestServer::new().assert_response(
      "/output/foo:0",
      StatusCode::BAD_REQUEST,
      "Invalid URL: error parsing TXID",
    );
  }

  #[test]
  fn home() {
    let test_server = TestServer::new();

    test_server.bitcoin_rpc_server.mine_blocks(1);

    test_server.assert_response_regex(
    "/",
    StatusCode::OK,
    ".*<title>Ordinals</title>.*
<h2>Status</h2>
<dl>
  <dt>cycle</dt><dd>0</dd>
  <dt>epoch</dt><dd>0</dd>
  <dt>period</dt><dd>0</dd>
  <dt>block</dt><dd>1</dd>
</dl>
<h2>Latest Blocks</h2>
<ol start=1 reversed class=blocks>
  <li><a href=/block/[[:xdigit:]]{64}>[[:xdigit:]]{64}</a></li>
  <li><a href=/block/000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f>000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f</a></li>
</ol>.*",
  );
  }

  #[test]
  fn nav_displays_chain() {
    TestServer::new().assert_response_regex(
      "/",
      StatusCode::OK,
      ".*<a href=/>Ordinals<sup>regtest</sup></a>.*",
    );
  }

  #[test]
  fn home_block_limit() {
    let test_server = TestServer::new();

    test_server.bitcoin_rpc_server.mine_blocks(101);

    test_server.assert_response_regex(
    "/",
    StatusCode::OK,
    ".*<ol start=101 reversed class=blocks>\n(  <li><a href=/block/[[:xdigit:]]{64}>[[:xdigit:]]{64}</a></li>\n){100}</ol>.*"
  );
  }

  #[test]
  fn block_not_found() {
    TestServer::new().assert_response(
      "/block/467a86f0642b1d284376d13a98ef58310caa49502b0f9a560ee222e0a122fe16",
      StatusCode::NOT_FOUND,
      "block 467a86f0642b1d284376d13a98ef58310caa49502b0f9a560ee222e0a122fe16 unknown",
    );
  }

  #[test]
  fn unmined_sat() {
    TestServer::new().assert_response_regex(
      "/sat/0",
      StatusCode::OK,
      ".*<dt>time</dt><dd>2009-01-03 18:15:05</dd>.*",
    );
  }

  #[test]
  fn mined_sat() {
    TestServer::new().assert_response_regex(
      "/sat/5000000000",
      StatusCode::OK,
      ".*<dt>time</dt><dd>.* \\(expected\\)</dd>.*",
    );
  }

  #[test]
  fn static_asset() {
    TestServer::new().assert_response_regex(
      "/static/index.css",
      StatusCode::OK,
      r".*\.rare \{
  background-color: var\(--rare\);
}.*",
    );
  }

  #[test]
  fn favicon() {
    TestServer::new().assert_response_regex("/favicon.ico", StatusCode::OK, r".*");
  }

  #[test]
  fn clock_updates() {
    let test_server = TestServer::new();
    test_server.assert_response_regex("/clock", StatusCode::OK, ".*<text.*>0</text>.*");
    test_server.bitcoin_rpc_server.mine_blocks(1);
    test_server.assert_response_regex("/clock", StatusCode::OK, ".*<text.*>1</text>.*");
  }

  #[test]
  fn block_by_hash() {
    let test_server = TestServer::new();

    test_server.bitcoin_rpc_server.mine_blocks(1);
    let transaction = TransactionTemplate {
      input_slots: &[(1, 0, 0)],
      output_count: 1,
      fee: 0,
    };
    test_server.bitcoin_rpc_server.broadcast_tx(transaction);
    let block_hash = test_server.bitcoin_rpc_server.mine_blocks(1)[0].block_hash();

    test_server.assert_response_regex(
      &format!("/block/{block_hash}"),
      StatusCode::OK,
      ".*<h1>Block 2</h1>
<dl>
  <dt>hash</dt><dd class=monospace>[[:xdigit:]]{64}</dd>
  <dt>target</dt><dd class=monospace>[[:xdigit:]]{64}</dd>
  <dt>timestamp</dt><dd>0</dd>
  <dt>size</dt><dd>203</dd>
  <dt>weight</dt><dd>812</dd>
  <dt>previous blockhash</dt><dd><a href=/block/659f9b67fbc0b5cba0ef6ebc0aea322e1c246e29e43210bd581f5f3bd36d17bf class=monospace>659f9b67fbc0b5cba0ef6ebc0aea322e1c246e29e43210bd581f5f3bd36d17bf</a></dd>
</dl>
<a href=/block/1>prev</a>
next
<h2>2 Transactions</h2>
<ul class=monospace>
  <li><a href=/tx/[[:xdigit:]]{64}>[[:xdigit:]]{64}</a></li>
  <li><a href=/tx/[[:xdigit:]]{64}>[[:xdigit:]]{64}</a></li>
</ul>.*",
    );
  }

  #[test]
  fn block_by_height() {
    let test_server = TestServer::new();

    test_server.assert_response_regex(
      "/block/0",
      StatusCode::OK,
      ".*<h1>Block 0</h1>
<dl>
  <dt>hash</dt><dd class=monospace>[[:xdigit:]]{64}</dd>
  <dt>target</dt><dd class=monospace>[[:xdigit:]]{64}</dd>
  <dt>timestamp</dt><dd>1231006505</dd>
  <dt>size</dt><dd>285</dd>
  <dt>weight</dt><dd>1140</dd>
</dl>
prev
next.*",
    );
  }

  #[test]
  fn transaction() {
    let test_server = TestServer::new();

    let coinbase_tx = test_server.bitcoin_rpc_server.mine_blocks(1)[0].txdata[0].clone();
    let txid = coinbase_tx.txid();

    test_server.assert_response_regex(
      &format!("/tx/{txid}"),
      StatusCode::OK,
      &format!(
        ".*<title>Transaction {txid}</title>.*<h1>Transaction <span class=monospace>{txid}</span></h1>
<h2>1 Output</h2>
<ul class=monospace>
  <li>
    <a href=/output/0c4eb1fa83a7d6ce0e21e5e616a96e83a7b1658170fb544acf6f5c6a2d4b3f90:0 class=monospace>
      0c4eb1fa83a7d6ce0e21e5e616a96e83a7b1658170fb544acf6f5c6a2d4b3f90:0
    </a>
    <dl>
      <dt>value</dt><dd>5000000000</dd>
      <dt>script pubkey</dt><dd class=data></dd>
    </dl>
  </li>
</ul>.*"
      ),
    );
  }

  #[test]
  fn detect_reorg() {
    let test_server = TestServer::new();

    test_server.bitcoin_rpc_server.mine_blocks(1);

    test_server.assert_response("/status", StatusCode::OK, "OK");

    test_server.bitcoin_rpc_server.invalidate_tip();
    test_server.bitcoin_rpc_server.mine_blocks(2);

    test_server.assert_response_regex("/status", StatusCode::OK, "reorg detected.*");
  }

  #[test]
  fn rare_with_index() {
    TestServer::new_with_args(&["--index-sats"]).assert_response(
      "/rare.txt",
      StatusCode::OK,
      "sat\tsatpoint
0\t4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0:0
",
    );
  }

  #[test]
  fn rare_without_satoshi_index() {
    TestServer::new_with_args(&[]).assert_response(
      "/rare.txt",
      StatusCode::NOT_FOUND,
      "tracking rare sats requires index created with `--index-sats` flag",
    );
  }

  #[test]
  fn show_rare_txt_in_header_with_satoshi_index() {
    TestServer::new_with_args(&["--index-sats"]).assert_response_regex(
      "/",
      StatusCode::OK,
      ".*
      <a href=/clock>Clock</a>
      <a href=/rare.txt>rare.txt</a>
      <form action=/search method=get>.*",
    );
  }

  #[test]
  fn rare_sat_location() {
    TestServer::new_with_args(&["--index-sats"]).assert_response_regex(
      "/sat/0",
      StatusCode::OK,
      ".*>4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0:0<.*",
    );
  }

  #[test]
  fn dont_show_rare_txt_in_header_without_satoshi_index() {
    TestServer::new().assert_response_regex(
      "/",
      StatusCode::OK,
      ".*
      <a href=/clock>Clock</a>
      <form action=/search method=get>.*",
    );
  }

  #[test]
  fn input() {
    TestServer::new().assert_response_regex(
      "/input/0/0/0",
      StatusCode::OK,
      ".*<title>Input /0/0/0</title>.*<h1>Input /0/0/0</h1>.*<dt>text</dt><dd>.*The Times 03/Jan/2009 Chancellor on brink of second bailout for banks</dd>.*",
    );
  }

  #[test]
  fn input_missing() {
    TestServer::new().assert_response(
      "/input/1/1/1",
      StatusCode::NOT_FOUND,
      "input /1/1/1 unknown",
    );
  }

  #[test]
  fn commits_are_tracked() {
    let server = TestServer::new();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::Commits)
        .unwrap(),
      1
    );

    let info = server.index.info().unwrap();
    assert_eq!(info.transactions.len(), 1);
    assert_eq!(info.transactions[0].starting_block_count, 0);

    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::Commits)
        .unwrap(),
      1
    );

    let info = server.index.info().unwrap();
    assert_eq!(info.transactions.len(), 1);
    assert_eq!(info.transactions[0].starting_block_count, 0);

    server.bitcoin_rpc_server.mine_blocks(1);

    thread::sleep(Duration::from_millis(10));
    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::Commits)
        .unwrap(),
      2
    );

    let info = server.index.info().unwrap();
    assert_eq!(info.transactions.len(), 2);
    assert_eq!(info.transactions[0].starting_block_count, 0);
    assert_eq!(info.transactions[1].starting_block_count, 1);
    assert!(
      info.transactions[1].starting_timestamp - info.transactions[0].starting_timestamp >= 10
    );
  }

  #[test]
  fn outputs_traversed_are_tracked() {
    let server = TestServer::new_with_args(&["--index-sats"]);

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::OutputsTraversed)
        .unwrap(),
      1
    );

    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::OutputsTraversed)
        .unwrap(),
      1
    );

    server.bitcoin_rpc_server.mine_blocks(1);
    server.bitcoin_rpc_server.mine_blocks(1);

    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::OutputsTraversed)
        .unwrap(),
      3
    );
  }

  #[test]
  fn coinbase_sat_ranges_are_tracked() {
    let server = TestServer::new_with_args(&["--index-sats"]);

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      1
    );

    server.bitcoin_rpc_server.mine_blocks(1);
    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      2
    );

    server.bitcoin_rpc_server.mine_blocks(1);
    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      3
    );
  }

  #[test]
  fn split_sat_ranges_are_tracked() {
    let server = TestServer::new_with_args(&["--index-sats"]);

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      1
    );

    server.bitcoin_rpc_server.mine_blocks(1);
    server.bitcoin_rpc_server.broadcast_tx(TransactionTemplate {
      input_slots: &[(1, 0, 0)],
      output_count: 2,
      fee: 0,
    });
    server.bitcoin_rpc_server.mine_blocks(1);
    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      4,
    );
  }

  #[test]
  fn fee_sat_ranges_are_tracked() {
    let server = TestServer::new_with_args(&["--index-sats"]);

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      1
    );

    server.bitcoin_rpc_server.mine_blocks(1);
    server.bitcoin_rpc_server.broadcast_tx(TransactionTemplate {
      input_slots: &[(1, 0, 0)],
      output_count: 2,
      fee: 2,
    });
    server.bitcoin_rpc_server.mine_blocks(1);
    server.index.update().unwrap();

    assert_eq!(
      server
        .index
        .statistic(crate::index::Statistic::SatRanges)
        .unwrap(),
      5,
    );
  }

  #[test]
  fn content_response_no_content() {
    assert_eq!(
      Server::content_response(Inscription::new(
        Some("text/plain".as_bytes().to_vec()),
        None
      )),
      None
    );
  }

  #[test]
  fn content_response_with_content() {
    assert_eq!(
      Server::content_response(Inscription::new(
        Some("text/plain".as_bytes().to_vec()),
        Some(vec![1, 2, 3]),
      )),
      Some(("text/plain".into(), vec![1, 2, 3]))
    );
  }

  #[test]
  fn content_response_no_content_type() {
    assert_eq!(
      Server::content_response(Inscription::new(None, Some(vec![]))),
      Some(("application/octet-stream".into(), vec![]))
    );
  }
}
