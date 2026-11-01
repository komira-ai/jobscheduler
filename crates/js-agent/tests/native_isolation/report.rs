//! The tables the test prints: what run A seeded, and what survived.

/// `rows` under `header`, one line each, the columns padded to the widest cell. The last
/// column is not padded. With no rows, only the header.
pub fn table(header: [&str; 3], rows: &[[String; 3]]) -> String {
    let mut widths = header.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: [&str; 3]| {
        format!(
            "{:<w0$}  {:<w1$}  {}",
            cells[0],
            cells[1],
            cells[2],
            w0 = widths[0],
            w1 = widths[1]
        )
        .trim_end()
        .to_owned()
    };
    let mut out = vec![line(header)];
    out.extend(rows.iter().map(|row| line([&row[0], &row[1], &row[2]])));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(a: &str, b: &str, c: &str) -> [String; 3] {
        [a.to_owned(), b.to_owned(), c.to_owned()]
    }

    /// Asserts the layout the Mac run prints: the header first, two spaces between
    /// columns, each column as wide as its widest cell, the last unpadded and no
    /// trailing space. It catches a table whose columns drift, which is unreadable
    /// when several things survived.
    #[test]
    fn the_columns_are_as_wide_as_their_widest_cell() {
        let rows = [
            row("sysv-semaphore", "key=5", "id 196608"),
            row("process", "pid=4242", ""),
        ];
        let text = table(["KIND", "WHERE", "DETAIL"], &rows);
        assert_eq!(
            text,
            "KIND            WHERE     DETAIL\n\
             sysv-semaphore  key=5     id 196608\n\
             process         pid=4242"
        );
    }

    /// Asserts that a table with no rows is its header alone. It catches a table that
    /// prints an empty line or panics when nothing survived, the case a pass ends on.
    #[test]
    fn no_rows_is_the_header_alone() {
        assert_eq!(
            table(["KIND", "WHERE", "DETAIL"], &[]),
            "KIND  WHERE  DETAIL"
        );
    }
}
