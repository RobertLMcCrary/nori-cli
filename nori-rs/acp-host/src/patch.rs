/// The line of the file at `path` (relative to `cwd`) that an edit's text begins on, counting
/// from 1: where `old_text` is in the file, or `new_text` once the edit has been written. An
/// edit's texts are often only the part of the file it replaced; when the file cannot be read or
/// holds neither, the texts are taken to begin the file.
pub fn first_line_in_file(
    path: &std::path::Path,
    cwd: &std::path::Path,
    old_text: &str,
    new_text: &str,
) -> usize {
    let full_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    std::fs::read_to_string(&full_path)
        .ok()
        .and_then(|file_content| {
            let offset = file_content
                .find(old_text)
                .or_else(|| file_content.find(new_text))?;
            Some(file_content[..offset].lines().count() + 1)
        })
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn first_line_in_file_uses_new_text_when_file_is_already_updated() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let file_path = temp_dir.path().join("story.txt");
        let file_content = (1..=100)
            .map(|line| {
                if line == 50 {
                    "line 50 updated\n".to_string()
                } else {
                    format!("line {line}\n")
                }
            })
            .collect::<String>();
        std::fs::write(&file_path, file_content).expect("write updated file");

        let first_line = first_line_in_file(
            std::path::Path::new("story.txt"),
            temp_dir.path(),
            "line 50\n",
            "line 50 updated\n",
        );

        assert_eq!(first_line, 50);
    }

    #[test]
    fn first_line_in_file_is_the_first_line_when_the_file_is_gone() {
        let temp_dir = tempfile::tempdir().expect("temp dir");

        let first_line = first_line_in_file(
            std::path::Path::new("missing.txt"),
            temp_dir.path(),
            "old\n",
            "new\n",
        );

        assert_eq!(first_line, 1);
    }
}
