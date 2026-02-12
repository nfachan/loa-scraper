use anyhow::{Result, anyhow};
use clap::Parser;
use colored::*;
use csv::Writer;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Serialize;
use std::fs::File;
use std::io::{self, Write};

#[derive(Parser, Debug)]
#[command(name = "loa-scraper")]
#[command(about = "Scrape Library of America volumes and generate CSV")]
struct Args {
    #[arg(short, long, help = "Starting volume number (default: 1)")]
    start: Option<u32>,

    #[arg(short, long, help = "Ending volume number (default: last available)")]
    end: Option<u32>,

    #[arg(short, long, help = "Output CSV file path (default: stdout)")]
    output: Option<String>,
}

#[derive(Debug, Serialize)]
struct Volume {
    volume_number: u32,
    title: String,
    author: String,
    author_wikipedia_link: String,
    loa_detail_link: String,
    original_volume_name: String,
    own_volume: String,
}

async fn scrape_collection_page(client: &Client) -> Result<Html> {
    let url = "https://www.loa.org/books/loa_collection/";
    let response = client.get(url).send().await?;
    let body = response.text().await?;

    Ok(Html::parse_document(&body))
}

async fn get_wikipedia_link(client: &Client, author: &str) -> Result<String> {
    // Skip if no author or if it's not a real author name
    if author.is_empty() || author == "Unknown" {
        return Ok(String::new());
    }

    let search_url = format!(
        "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit=1&format=json",
        urlencoding::encode(author)
    );

    match client
        .get(&search_url)
        .header(
            "User-Agent",
            "LOA-Scraper/1.0 (https://github.com/example/loa-scraper)",
        )
        .send()
        .await
    {
        Ok(response) => {
            match response.text().await {
                Ok(text) => {
                    if text.trim().is_empty() {
                        return Ok(String::new());
                    }

                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(json) => {
                            // OpenSearch API returns: [query, [titles], [descriptions], [urls]]
                            if let Some(urls) = json.get(3).and_then(|v| v.as_array())
                                && let Some(url) = urls.first().and_then(|v| v.as_str())
                                    && !url.is_empty() {
                                        return Ok(url.to_string());
                                    }
                        }
                        Err(_) => {
                            // If JSON parsing fails, it might be an error page - just return empty
                            return Ok(String::new());
                        }
                    }
                }
                Err(_) => {
                    return Ok(String::new());
                }
            }
        }
        Err(_) => {
            return Ok(String::new());
        }
    }

    Ok(String::new())
}

fn is_likely_author(text: &str) -> bool {
    // Heuristics to determine if text is likely an author name vs. a series/collection title

    // If it contains "The " at the start, it's more likely a title
    if text.starts_with("The ") {
        return false;
    }

    // Common patterns that indicate it's NOT an author name
    let non_author_patterns = [
        "The American Short Story",
        "The Best American",
        "American Poetry",
        "Collected Works",
        "Complete Works",
        "Selected Works",
        "Early Works",
        "Later Works",
        "Writings",
        "Letters",
        "Speeches",
        "Documents",
        "Chronicles",
        "Anthology",
        "Collection",
    ];

    for pattern in &non_author_patterns {
        if text.contains(pattern) {
            return false;
        }
    }

    // If it looks like "Firstname Lastname" or "F. Lastname" or "Firstname M. Lastname", it's likely an author
    let words: Vec<&str> = text.split_whitespace().collect();

    // Single word is unlikely to be an author (unless it's like "Aristotle")
    if words.len() == 1 {
        // Some single-name authors exist, but let's be conservative
        return text.chars().any(|c| c.is_lowercase()); // Has lowercase letters (not all caps title)
    }

    // Two or more words - check if it looks like a name
    if words.len() >= 2 {
        let first_word = words[0];
        let last_word = words[words.len() - 1];

        // Check if first and last words start with capital letters (name pattern)
        if first_word
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase())
            && last_word.chars().next().is_some_and(|c| c.is_uppercase())
        {
            // Additional check: avoid things like "Civil War" or "New England"
            if words.len() == 2
                && (text.contains("War")
                    || text.contains("American")
                    || text.contains("New ")
                    || text.contains("Old "))
            {
                return false;
            }

            return true;
        }
    }

    false
}

type VolumeData = (u32, String, String, String, String);

fn parse_volumes(html: &Html) -> Result<Vec<VolumeData>> {
    let book_listing_selector = Selector::parse("li.content-listing.content-listing--book")
        .map_err(|e| anyhow!("CSS selector error: {:?}", e))?;
    let link_selector = Selector::parse("a").map_err(|e| anyhow!("CSS selector error: {:?}", e))?;
    let number_selector = Selector::parse("i.book-listing__number")
        .map_err(|e| anyhow!("CSS selector error: {:?}", e))?;
    let title_selector = Selector::parse("b.content-listing__title")
        .map_err(|e| anyhow!("CSS selector error: {:?}", e))?;
    let mut volumes = Vec::new();

    for book_element in html.select(&book_listing_selector) {
        let link_element = book_element.select(&link_selector).next();
        let number_element = book_element.select(&number_selector).next();
        let title_element = book_element.select(&title_selector).next();

        if let (Some(link), Some(number), Some(title)) =
            (link_element, number_element, title_element)
        {
            let href = link.value().attr("href").unwrap_or("");
            let volume_number = number
                .text()
                .collect::<String>()
                .trim()
                .parse::<u32>()
                .unwrap_or(0);
            let title_text = title.text().collect::<String>().trim().to_string();

            if volume_number > 0 {
                // Parse title which could be "Author: Title" or "Series Title: Subtitle"
                let (author, book_title) = if let Some(colon_pos) = title_text.find(':') {
                    let before_colon = title_text[..colon_pos].trim();
                    let after_colon = title_text[colon_pos + 1..].trim();

                    if is_likely_author(before_colon) {
                        // It's an author: use as author and title
                        (before_colon.to_string(), after_colon.to_string())
                    } else {
                        // It's likely a series or collection title: treat whole thing as title
                        (String::new(), title_text.clone())
                    }
                } else {
                    // No colon found: treat as title with unknown author
                    (String::new(), title_text.clone())
                };

                volumes.push((
                    volume_number,
                    book_title,
                    author,
                    href.to_string(),
                    title_text.clone(),
                ));
            }
        }
    }

    volumes.sort_by_key(|v| v.0);
    Ok(volumes)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let start_volume = args.start.unwrap_or(1);

    eprintln!(
        "{} {}",
        "🔍".cyan(),
        "Scraping Library of America volumes".bright_blue().bold()
    );

    let client = Client::new();

    // Create spinner for fetching page
    eprintln!(
        "{} {}",
        "📡".yellow(),
        "Fetching collection page...".yellow()
    );
    let html = scrape_collection_page(&client).await?;

    eprintln!("{} {}", "📚".green(), "Parsing volumes...".green());
    let volumes_data = parse_volumes(&html)?;

    // Filter by start and end volume
    let filtered_volumes: Vec<_> = volumes_data
        .into_iter()
        .filter(|(num, _, _, _, _)| {
            *num >= start_volume && args.end.is_none_or(|end| *num <= end)
        })
        .collect();

    let volume_range = if let Some(end) = args.end {
        format!("{}-{}", start_volume, end)
    } else {
        format!("{}+", start_volume)
    };

    eprintln!(
        "{} {} volumes {} (volumes {})",
        "✅".green(),
        "Found".green().bold(),
        filtered_volumes.len().to_string().bright_white().bold(),
        volume_range.cyan()
    );

    if filtered_volumes.is_empty() {
        eprintln!(
            "{} {}",
            "⚠️".yellow(),
            "No volumes found in specified range".yellow()
        );
        return Ok(());
    }

    // Setup output writer
    let mut writer: Writer<Box<dyn Write>> = if let Some(output_path) = &args.output {
        let file = File::create(output_path)?;
        Writer::from_writer(Box::new(file))
    } else {
        Writer::from_writer(Box::new(io::stdout()))
    };

    // Progress bar for processing
    let pb = ProgressBar::new(filtered_volumes.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
    );

    eprintln!(
        "{} {}",
        "🔗".magenta(),
        "Processing volumes and finding Wikipedia links...".magenta()
    );

    for (i, (volume_number, title, author, loa_link, original_name)) in
        filtered_volumes.iter().enumerate()
    {
        pb.set_message(format!(
            "Volume {}: {}",
            volume_number,
            title.chars().take(40).collect::<String>()
        ));

        if i > 0 && i % 10 == 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        let wikipedia_link: String = get_wikipedia_link(&client, author).await.unwrap_or_default();

        let volume = Volume {
            volume_number: *volume_number,
            title: title.clone(),
            author: author.clone(),
            author_wikipedia_link: wikipedia_link,
            loa_detail_link: loa_link.clone(),
            original_volume_name: original_name.clone(),
            own_volume: String::new(),
        };

        writer.serialize(&volume)?;
        pb.inc(1);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    pb.finish_with_message("Complete!");
    writer.flush()?;

    if let Some(output_path) = &args.output {
        eprintln!(
            "{} {} '{}'",
            "💾".green(),
            "CSV file created successfully:".green().bold(),
            output_path.bright_white()
        );
    }

    Ok(())
}
