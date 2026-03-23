use std::{
    env, fs,
    io::{self, Write},
};

use anyhow::{anyhow, bail, Context, Result};
use pn_polymarket::{GammaClient, GammaMarket};

const DEFAULT_CONFIG_PATH: &str = "config/default.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
enum LookupTarget {
    ConditionId(String),
    EventSlug(String),
    MarketPath {
        event_slug: String,
        market_slug: String,
    },
}

struct CliArgs {
    input: String,
    config_path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_cli_args(env::args().skip(1).collect())?;
    let client = GammaClient::new();
    let target = classify_input(&args.input);

    let selected = match &target {
        LookupTarget::ConditionId(condition_id) => {
            if let Some(market) = client.get_market(condition_id).await? {
                println!("Resolved condition_id to market:");
                print_market(1, &market);
                confirm_or_abort("Write this condition_id to config?")?;
            } else {
                println!("Condition ID not found via Gamma. Writing it directly.");
                confirm_or_abort("Write this raw condition_id to config?")?;
            }
            Selection {
                condition_id: condition_id.clone(),
                question: None,
                slug: None,
            }
        }
        _ => {
            let markets = resolve_markets(&client, &target).await?;
            if markets.is_empty() {
                bail!("no candidate markets found for '{}'", args.input);
            }
            choose_market(markets)?
        }
    };

    write_condition_id_to_file(&args.config_path, &selected.condition_id)?;

    println!("Updated {}", args.config_path);
    if let Some(question) = &selected.question {
        println!("Question: {question}");
    }
    if let Some(slug) = &selected.slug {
        println!("Slug: {slug}");
    }
    println!("Condition ID: {}", selected.condition_id);
    println!("Restart poly-lp for the new target to take effect.");
    Ok(())
}

#[derive(Debug, Clone)]
struct Selection {
    condition_id: String,
    question: Option<String>,
    slug: Option<String>,
}

fn parse_cli_args(args: Vec<String>) -> Result<CliArgs> {
    let mut config_path = DEFAULT_CONFIG_PATH.to_string();
    let mut input = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let path = iter
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a path"))?;
                config_path = path;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            _ if input.is_none() => input = Some(arg),
            _ => bail!("unexpected argument '{arg}'"),
        }
    }

    let input = input
        .ok_or_else(|| anyhow!("missing input: provide a Polymarket URL, slug, or condition_id"))?;

    Ok(CliArgs { input, config_path })
}

fn print_usage() {
    println!("Usage: poly-target [--config path] <polymarket-url|slug|condition-id>");
    println!(
        "Example: cargo run -p poly-lp --bin poly-target -- \"https://polymarket.com/event/...\""
    );
}

fn classify_input(input: &str) -> LookupTarget {
    let trimmed = input.trim().trim_matches('/');
    if trimmed.starts_with("0x") {
        return LookupTarget::ConditionId(trimmed.to_string());
    }

    if let Some(after) = extract_event_path(trimmed) {
        return classify_event_path(after);
    }

    if trimmed.matches('/').count() == 1 && !trimmed.contains(' ') {
        let mut parts = trimmed.split('/').filter(|segment| !segment.is_empty());
        if let (Some(event_slug), Some(market_slug)) = (parts.next(), parts.next()) {
            return LookupTarget::MarketPath {
                event_slug: event_slug.to_string(),
                market_slug: market_slug.to_string(),
            };
        }
    }

    LookupTarget::EventSlug(trimmed.to_string())
}

fn extract_event_path(input: &str) -> Option<&str> {
    let idx = input.find("/event/")?;
    let after = &input[idx + "/event/".len()..];
    let clean = after.split(&['?', '#'][..]).next()?;
    Some(clean.trim_matches('/'))
}

fn classify_event_path(path: &str) -> LookupTarget {
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    match (segments.next(), segments.next()) {
        (Some(event_slug), Some(market_slug)) => LookupTarget::MarketPath {
            event_slug: event_slug.to_string(),
            market_slug: market_slug.to_string(),
        },
        (Some(event_slug), None) => LookupTarget::EventSlug(event_slug.to_string()),
        _ => LookupTarget::EventSlug(path.to_string()),
    }
}

async fn resolve_markets(client: &GammaClient, target: &LookupTarget) -> Result<Vec<GammaMarket>> {
    let mut markets = match target {
        LookupTarget::ConditionId(condition_id) => client
            .get_market(condition_id)
            .await?
            .into_iter()
            .collect::<Vec<_>>(),
        LookupTarget::EventSlug(slug) => client.get_market_by_slug(slug).await?,
        LookupTarget::MarketPath {
            event_slug,
            market_slug,
        } => {
            let markets = client.get_market_by_slug(event_slug).await?;
            let exact_matches = markets
                .iter()
                .filter(|market| market.slug == *market_slug)
                .cloned()
                .collect::<Vec<_>>();
            if exact_matches.is_empty() {
                markets
            } else {
                exact_matches
            }
        }
    };

    markets.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then_with(|| left.question.cmp(&right.question))
    });
    Ok(markets)
}

fn choose_market(markets: Vec<GammaMarket>) -> Result<Selection> {
    if markets.len() == 1 {
        let market = &markets[0];
        println!("Resolved one candidate market:");
        print_market(1, market);
        confirm_or_abort("Write this condition_id to config?")?;
        return Ok(selection_from_market(market));
    }

    println!("Resolved {} candidate markets:", markets.len());
    for (index, market) in markets.iter().enumerate() {
        print_market(index + 1, market);
    }

    let choice = prompt_choice(markets.len())?;
    let market = &markets[choice - 1];
    confirm_or_abort("Write the selected condition_id to config?")?;
    Ok(selection_from_market(market))
}

fn selection_from_market(market: &GammaMarket) -> Selection {
    Selection {
        condition_id: market.condition_id.clone(),
        question: Some(market.question.clone()),
        slug: Some(market.slug.clone()),
    }
}

fn print_market(index: usize, market: &GammaMarket) {
    let status = if market.active { "active" } else { "inactive" };
    println!(
        "[{index}] {status} | {} | slug={} | condition_id={}",
        market.question, market.slug, market.condition_id
    );
}

fn prompt_choice(max: usize) -> Result<usize> {
    print!("Select market [1-{max}]: ");
    io::stdout()
        .flush()
        .context("flush stdout for selection prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read selection from stdin")?;

    let choice = input
        .trim()
        .parse::<usize>()
        .context("selection must be a number")?;
    if !(1..=max).contains(&choice) {
        bail!("selection must be between 1 and {max}");
    }
    Ok(choice)
}

fn confirm_or_abort(prompt: &str) -> Result<()> {
    print!("{prompt} [y/N]: ");
    io::stdout()
        .flush()
        .context("flush stdout for confirmation prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read confirmation from stdin")?;

    let normalized = input.trim().to_ascii_lowercase();
    if normalized == "y" || normalized == "yes" {
        return Ok(());
    }

    bail!("aborted by operator")
}

fn write_condition_id_to_file(path: &str, condition_id: &str) -> Result<()> {
    let existing =
        fs::read_to_string(path).with_context(|| format!("failed to read config file '{path}'"))?;
    let updated = replace_condition_id_in_config(&existing, condition_id)?;
    fs::write(path, updated).with_context(|| format!("failed to write config file '{path}'"))?;
    Ok(())
}

fn replace_condition_id_in_config(contents: &str, new_condition_id: &str) -> Result<String> {
    let mut output = Vec::new();
    let mut in_lp_trading = false;
    let mut saw_lp_trading = false;
    let mut replaced = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_lp_trading = trimmed == "[lp.trading]";
            saw_lp_trading |= in_lp_trading;
            output.push(line.to_string());
            continue;
        }

        if in_lp_trading && trimmed.starts_with("condition_id") {
            let indent_width = line.find("condition_id").unwrap_or(0);
            let indent = &line[..indent_width];
            let trailing_comment = line
                .find('#')
                .map(|index| format!(" {}", line[index..].trim_start()))
                .unwrap_or_default();
            output.push(format!(
                "{indent}condition_id = \"{new_condition_id}\"{trailing_comment}"
            ));
            replaced = true;
            continue;
        }

        output.push(line.to_string());
    }

    if !saw_lp_trading {
        bail!("config file does not contain [lp.trading]");
    }
    if !replaced {
        bail!("config file does not contain lp.trading.condition_id");
    }

    let mut rendered = output.join("\n");
    if contents.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::{classify_input, replace_condition_id_in_config, LookupTarget};

    #[test]
    fn classify_input_recognizes_event_url() {
        let target = classify_input("https://polymarket.com/event/will-fed-cut-rates");
        assert_eq!(
            target,
            LookupTarget::EventSlug("will-fed-cut-rates".to_string())
        );
    }

    #[test]
    fn classify_input_recognizes_market_url() {
        let target = classify_input(
            "https://polymarket.com/event/will-fed-cut-rates/will-fed-cut-rates-in-june",
        );
        assert_eq!(
            target,
            LookupTarget::MarketPath {
                event_slug: "will-fed-cut-rates".to_string(),
                market_slug: "will-fed-cut-rates-in-june".to_string(),
            }
        );
    }

    #[test]
    fn classify_input_recognizes_slug_and_condition_id() {
        assert_eq!(
            classify_input("will-fed-cut-rates"),
            LookupTarget::EventSlug("will-fed-cut-rates".to_string())
        );
        assert_eq!(
            classify_input("0xabc123"),
            LookupTarget::ConditionId("0xabc123".to_string())
        );
    }

    #[test]
    fn replace_condition_id_in_lp_trading_section() {
        let input = r#"[database]
url = "sqlite:poly-notifier.db"

[lp.trading]
condition_id = "replace-me"
gamma_base_url = "https://gamma-api.polymarket.com"

[lp.strategy]
quote_size = 10.0
"#;
        let updated = replace_condition_id_in_config(input, "0xnew-target")
            .expect("condition id should be updated");
        assert!(updated.contains("condition_id = \"0xnew-target\""));
        assert!(updated.contains("gamma_base_url = \"https://gamma-api.polymarket.com\""));
        assert!(updated.contains("[lp.strategy]"));
    }

    #[test]
    fn replace_condition_id_errors_without_lp_trading_section() {
        let input = r#"[database]
url = "sqlite:poly-notifier.db"
"#;
        assert!(replace_condition_id_in_config(input, "0xnew-target").is_err());
    }

    #[test]
    fn replace_condition_id_errors_without_condition_id_key() {
        let input = r#"[lp.trading]
gamma_base_url = "https://gamma-api.polymarket.com"
"#;
        assert!(replace_condition_id_in_config(input, "0xnew-target").is_err());
    }
}
