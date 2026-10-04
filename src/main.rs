//! Interactive REPL. In M0 it only parses commands; M1 wires in the reference book.

use std::io::{self, BufRead, Write};

use lob::Command;

const HELP: &str = "\
commands:
  limit  <id> <buy|sell> <qty> <price>   prices are integer ticks
  market <id> <buy|sell> <qty>
  cancel <id>
  help | quit";

fn main() -> io::Result<()> {
    println!("lob (M0 scaffold: commands are parsed but not matched yet). Type `help`.");
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("> ");
        stdout.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        match line.trim() {
            "" => continue,
            "help" => println!("{HELP}"),
            "quit" | "exit" => return Ok(()),
            input => match input.parse::<Command>() {
                Ok(cmd) => println!("parsed: {cmd}"),
                Err(e) => println!("error: {e}"),
            },
        }
    }
}
