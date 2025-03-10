use super::*;

#[derive(Boilerplate)]
pub(crate) struct HomeHtml {
  last: u64,
  blocks: Vec<BlockHash>,
  starting_sat: Option<Sat>,
  inscriptions: Vec<(Inscription, InscriptionId)>,
}

impl HomeHtml {
  pub(crate) fn new(
    blocks: Vec<(u64, BlockHash)>,
    inscriptions: Vec<(Inscription, InscriptionId)>,
  ) -> Self {
    Self {
      starting_sat: blocks
        .get(0)
        .map(|(height, _)| Height(*height).starting_sat()),
      last: blocks
        .get(0)
        .map(|(height, _)| height)
        .cloned()
        .unwrap_or(0),
      blocks: blocks.into_iter().map(|(_, hash)| hash).collect(),
      inscriptions,
    }
  }
}

impl PageContent for HomeHtml {
  fn title(&self) -> String {
    "Ordinals".to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn html() {
    assert_regex_match!(
      &HomeHtml::new(
        vec![
          (
            1260001,
            "1111111111111111111111111111111111111111111111111111111111111111"
              .parse()
              .unwrap()
          ),
          (
            1260000,
            "0000000000000000000000000000000000000000000000000000000000000000"
              .parse()
              .unwrap()
          )
        ],
        vec![(
          inscription("text/plain;charset=utf-8", "HELLOWORLD"),
          txid(1)
        )],
      )
      .to_string(),
      "<h1>Bitcoin-native NFTs</h1>.*<h2>Latest Inscriptions</h2>
<div class=inscriptions>
  <a href=/inscription/1111111111111111111111111111111111111111111111111111111111111111><pre class=inscription>HELLOWORLD</pre></a>
</div>
<h2>Status</h2>
<dl>
  <dt>cycle</dt><dd>1</dd>
  <dt>epoch</dt><dd>6</dd>
  <dt>period</dt><dd>625</dd>
  <dt>block</dt><dd>1260001</dd>
</dl>
<h2>Latest Blocks</h2>
<ol start=1260001 reversed class=blocks>
  <li><a href=/block/1{64}>1{64}</a></li>
  <li><a href=/block/0{64}>0{64}</a></li>
</ol>
",
    );
  }
}
