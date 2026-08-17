//! AgentFlow Task Dispatcher CLI
//!
//! Dispatches YAML task files to a running AgentFlow server.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use glob::glob;

/// AgentFlow Task Dispatcher - Dispatches tasks to the system
#[derive(Parser, Debug)]
#[command(name = "agentflow-task-dispatcher")]
#[command(version = "0.1.0")]
#[command(about = "Dispatch development tasks to AgentFlow")]
struct Args {
    /// AgentFlow server URL
    #[arg(short, long, default_value = "http://localhost:8080")]
    server: String,
    
    /// Task file to submit
    #[arg(short, long)]
    task: Option<PathBuf>,
    
    /// Directory containing task files
    #[arg(short, long)]
    directory: Option<PathBuf>,
    
    /// Submit all tasks in default directory
    #[arg(short, long)]
    all: bool,
    
    /// Pattern for task files
    #[arg(short, long, default_value = "*.yaml")]
    pattern: String,
    
    /// Dry run - don't actually submit
    #[arg(long)]
    dry_run: bool,
    
    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Simple HTTP client
struct HttpClient {
    base_url: String,
    client: reqwest::Client,
}

impl HttpClient {
    fn new(base_url: String) -> Self {
        Self {
            base_url,
            client: reqwest::Client::new(),
        }
    }
    
    async fn check_health(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        let result = self.client.get(&url)
            .timeout(Duration::from_secs(5))
            .send().await;
        Ok(result.is_ok_and(|r| r.status().is_success()))
    }
    
    async fn submit_task(&self, yaml_content: &str) -> Result<String> {
        let url = format!("{}/api/tasks", self.base_url);
        let response = self.client.post(&url)
            .header("Content-Type", "application/yaml")
            .body(yaml_content.to_string())
            .timeout(Duration::from_secs(30))
            .send().await
            .context("Failed to submit task")?;
        
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Server error: {} - {}", status, body);
        }
        
        // Read response body
        response.text().await.context("Failed to read response")
    }
}

/// Extract metadata from task YAML (handles multi-document YAML)
fn extract_metadata(yaml: &str) -> (String, String, String) {
    // Parse each line to find top-level fields
    let mut id = "unknown".to_string();
    let mut title = "untitled".to_string();
    let mut task_type = "custom".to_string();
    
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("id:") {
            id = trimmed[3..].trim().trim_start_matches('"').trim_end_matches('"').to_string();
        } else if trimmed.starts_with("title:") {
            title = trimmed[6..].trim().trim_start_matches('"').trim_end_matches('"').to_string();
        } else if trimmed.starts_with("task_type:") || trimmed.starts_with("type:") {
            task_type = trimmed.split(':').nth(1).unwrap_or("custom").trim().trim_start_matches('"').trim_end_matches('"').to_string();
        }
    }
    
    (id, title, task_type)
}

/// Load task files matching pattern
fn load_task_files(directory: &PathBuf, pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern_str = format!("{}/{}", directory.display(), pattern);
    let mut files = Vec::new();
    
    for entry in glob(&pattern_str)? {
        if let Ok(path) = entry {
            if path.is_file() {
                files.push(path);
            }
        }
    }
    
    Ok(files)
}

fn print_header(text: &str) {
    println!("\n{}", "=".repeat(60));
    println!("  {}", text);
    println!("{}\n", "=".repeat(60));
}

fn print_task_info(file: &PathBuf, yaml: &str) {
    let (id, title, task_type) = extract_metadata(yaml);
    println!("  📝 {}", file.display());
    println!("     ID:    {}", id);
    println!("     Title: {}", title);
    println!("     Type:  {}", task_type);
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    print_header("AgentFlow Task Dispatcher");
    
    // Determine task files
    let task_files = if args.task.is_some() {
        vec![args.task.unwrap()]
    } else if args.directory.is_some() {
        let dir = args.directory.unwrap();
        if !dir.exists() {
            anyhow::bail!("Directory not found: {}", dir.display());
        }
        load_task_files(&dir, &args.pattern)?
    } else if args.all {
        let default_dir = PathBuf::from("tasks");
        if default_dir.exists() {
            load_task_files(&default_dir, &args.pattern)?
        } else {
            anyhow::bail!("Default tasks directory not found. Use --task or --directory.");
        }
    } else {
        anyhow::bail!("Please specify --task, --directory, or --all");
    };
    
    if task_files.is_empty() {
        anyhow::bail!("No task files found matching: {}", args.pattern);
    }
    
    println!("Server: {}", args.server);
    println!("Mode:   {}", if args.dry_run { "DRY RUN" } else { "LIVE" });
    println!("Tasks:  {} found\n", task_files.len());
    
    // Create client
    let client = HttpClient::new(args.server.clone());
    
    // Check server if not dry run
    if !args.dry_run {
        if !client.check_health().await? {
            anyhow::bail!(
                "Cannot connect to AgentFlow server at '{}'. Please start the server.",
                args.server
            );
        }
        println!("✓ Connected to server");
        println!();
    }
    
    // Process tasks
    let mut submitted = 0;
    let mut failed = 0;
    
    for task_file in &task_files {
        let yaml = std::fs::read_to_string(task_file)
            .with_context(|| format!("Failed to read: {}", task_file.display()))?;
        
        print_task_info(task_file, &yaml);
        
        if args.dry_run {
            println!("     → Would submit (dry run)\n");
            continue;
        }
        
        match client.submit_task(&yaml).await {
            Ok(response) => {
                submitted += 1;
                let response = response.trim();
                if !response.is_empty() {
                    println!("     ✓ Submitted - {}", response);
                } else {
                    println!("     ✓ Submitted");
                }
            }
            Err(e) => {
                failed += 1;
                println!("     ✗ Failed: {}", e);
            }
        }
        println!();
    }
    
    print_header("Summary");
    println!("  Total:   {}", task_files.len());
    println!("  ✓ Submitted: {}", submitted);
    println!("  ✗ Failed:    {}", failed);
    print_header("");
    
    if failed > 0 {
        anyhow::bail!("Some tasks failed");
    }
    
    Ok(())
}
