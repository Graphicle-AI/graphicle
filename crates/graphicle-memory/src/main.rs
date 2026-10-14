use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    let stream = std::env::args().any(|x| x == "--jsonl");
    let mut output = io::BufWriter::new(io::stdout().lock());
    if stream {
        return graphicle::protocol::jsonl(
            &mut io::stdin().lock(),
            &mut output,
            graphicle_memory::process,
        );
    } else {
        let mut request = Vec::new();
        io::stdin()
            .take(graphicle_memory::MAX_REQUEST_BYTES as u64 + 1)
            .read_to_end(&mut request)?;
        let result = output
            .write_all(&graphicle_memory::process(&request))
            .and_then(|()| output.write_all(b"\n"))
            .and_then(|()| output.flush());
        if let Err(error) = result
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(error);
        }
    }
    Ok(())
}
