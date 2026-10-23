// Copyright 2025 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use bstr::ByteSlice as _;
use futures::AsyncRead;
use futures::AsyncReadExt as _;
use futures::io::Cursor;

use jj_lib::config::ConfigGetError;
use jj_lib::settings::UserSettings;

fn is_binary(bytes: &[u8]) -> bool {
    // TODO(06393993): align the algorithm with git so that the git config autocrlf
    // users won't see different decisions on whether a file is binary and needs to
    // perform EOL conversion.
    let mut bytes = bytes.iter().peekable();
    while let Some(byte) = bytes.next() {
        match *byte {
            b'\0' => return true,
            b'\r' if bytes.peek() != Some(&&b'\n') => {
                return true;
            }
            _ => {}
        }
    }
    false
}

#[derive(Clone)]
pub(crate) struct TargetEolStrategy {
    eol_conversion_mode: EolConversionMode,
}

impl TargetEolStrategy {
    pub(crate) fn new(eol_conversion_mode: EolConversionMode) -> Self {
        Self {
            eol_conversion_mode,
        }
    }

    /// The limit to probe for whether the file is binary is 8KB.
    /// All files strictly smaller than the limit are always
    /// evaluated correctly and in full.
    /// Files larger than the limit - or with ambiguous content at the limit -
    /// are potentially misclassified.
    const PROBE_LIMIT: u64 = 8 << 10;

    /// Peek into the first [`TargetEolStrategy::PROBE_LIMIT`] bytes of content
    /// to determine if it is binary data.
    ///
    /// Peeked data is stored in `peek`.
    async fn probe_for_binary(
        mut contents: impl AsyncRead + Unpin,
        peek: &mut Vec<u8>,
    ) -> Result<bool, std::io::Error> {
        (&mut contents)
            .take(Self::PROBE_LIMIT)
            .read_to_end(peek)
            .await?;

        // The probe limit may have sliced a CRLF sequence, which would cause
        // misclassification as binary.
        let slice_to_check = if peek.get(Self::PROBE_LIMIT as usize - 1) == Some(&b'\r') {
            &peek[0..Self::PROBE_LIMIT as usize - 1]
        } else {
            peek
        };

        Ok(is_binary(slice_to_check))
    }

    pub(crate) async fn convert_eol_for_snapshot<'a>(
        &self,
        mut contents: impl AsyncRead + Send + Unpin + 'a,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin + 'a>, std::io::Error> {
        match self.eol_conversion_mode {
            EolConversionMode::None => Ok(Box::new(contents)),
            EolConversionMode::Input | EolConversionMode::InputOutput => {
                let mut peek = vec![];
                let target_eol = if Self::probe_for_binary(&mut contents, &mut peek).await? {
                    TargetEol::PassThrough
                } else {
                    TargetEol::Lf
                };
                let peek = Cursor::new(peek);
                let contents = peek.chain(contents);
                convert_eol(contents, target_eol).await
            }
        }
    }

    pub(crate) async fn convert_eol_for_update<'a>(
        &self,
        mut contents: impl AsyncRead + Send + Unpin + 'a,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin + 'a>, std::io::Error> {
        match self.eol_conversion_mode {
            EolConversionMode::None | EolConversionMode::Input => Ok(Box::new(contents)),
            EolConversionMode::InputOutput => {
                let mut peek = vec![];
                let target_eol = if Self::probe_for_binary(&mut contents, &mut peek).await? {
                    TargetEol::PassThrough
                } else {
                    TargetEol::Crlf
                };
                let peek = Cursor::new(peek);
                let contents = peek.chain(contents);
                convert_eol(contents, target_eol).await
            }
        }
    }
}

/// Configuring auto-converting CRLF line endings into LF when you add a file to
/// the backend, and vice versa when it checks out code onto your filesystem.
#[derive(Debug, PartialEq, Eq, Copy, Clone, serde::Deserialize)]
#[serde(rename_all(deserialize = "kebab-case"))]
pub enum EolConversionMode {
    /// Do not perform EOL conversion.
    None,
    /// Only perform the CRLF to LF EOL conversion when writing to the backend
    /// store from the file system.
    Input,
    /// Perform CRLF to LF EOL conversion when writing to the backend store from
    /// the file system and LF to CRLF EOL conversion when writing to the file
    /// system from the backend store.
    InputOutput,
}

impl EolConversionMode {
    /// Try to create the [`EolConversionMode`] based on the
    /// `working-copy.eol-conversion` setting in the [`UserSettings`].
    pub fn try_from_settings(user_settings: &UserSettings) -> Result<Self, ConfigGetError> {
        user_settings.get("working-copy.eol-conversion")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetEol {
    Lf,
    Crlf,
    PassThrough,
}

async fn convert_eol<'a>(
    mut input: impl AsyncRead + Send + Unpin + 'a,
    target_eol: TargetEol,
) -> Result<Box<dyn AsyncRead + Send + Unpin + 'a>, std::io::Error> {
    let eol = match target_eol {
        TargetEol::PassThrough => {
            return Ok(Box::new(input));
        }
        TargetEol::Lf => b"\n".as_slice(),
        TargetEol::Crlf => b"\r\n".as_slice(),
    };

    let mut contents = vec![];
    input.read_to_end(&mut contents).await?;
    let lines = contents.lines_with_terminator();
    let mut res = Vec::<u8>::with_capacity(contents.len());
    fn trim_last_eol(input: &[u8]) -> Option<&[u8]> {
        input
            .strip_suffix(b"\r\n")
            .or_else(|| input.strip_suffix(b"\n"))
    }
    for line in lines {
        if let Some(line) = trim_last_eol(line) {
            res.extend_from_slice(line);
            // If the line ends with an EOL, we should append the target EOL.
            res.extend_from_slice(eol);
        } else {
            // If the line doesn't end with an EOL, we don't append the EOL. This can happen
            // on the last line.
            res.extend_from_slice(line);
        }
    }
    Ok(Box::new(Cursor::new(res)))
}
