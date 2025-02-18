/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

/////////////////////////////////////////////////////////////////////
// DEMO USAGE
// Reading a file is based on the value in that file, plus the file encoding

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_futures::cancellation::CancellationContext;
use derive_more::Display;
use dice::DetectCycles;
use dice::Dice;
use dice::DiceComputations;
use dice::DiceTransactionUpdater;
use dice::InjectedKey;
use dice::Key;
use dupe::Dupe;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Dupe, PartialEq, Allocative)]
enum Encoding {
    Utf8,
    Ascii,
}

#[derive(Clone, Dupe, Debug, Display, Eq, Hash, PartialEq, Allocative)]
#[display(fmt = "{:?}", self)]
struct EncodingConfig();

impl InjectedKey for EncodingConfig {
    type Value = Encoding;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }
}

struct Encodings<'c>(&'c mut DiceComputations);

impl<'c> Encodings<'c> {
    async fn get(&mut self) -> Result<Encoding, Arc<anyhow::Error>> {
        self.0
            .compute(&EncodingConfig())
            .await
            .map_err(|e| Arc::new(anyhow::anyhow!(e)))
    }
}

trait HasEncodings {
    fn encodings(&mut self) -> Encodings;
}

impl HasEncodings for DiceComputations {
    fn encodings(&mut self) -> Encodings {
        Encodings(self)
    }
}

trait SetEncodings {
    fn set_encodings(&mut self, enc: Encoding) -> anyhow::Result<()>;
}

impl SetEncodings for DiceTransactionUpdater {
    fn set_encodings(&mut self, enc: Encoding) -> anyhow::Result<()> {
        Ok(self.changed_to(vec![(EncodingConfig(), enc)])?)
    }
}

struct Filesystem<'c>(&'c mut DiceComputations);

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative)]
#[display(fmt = "File({})", "_0.display()")]
struct File(PathBuf);

impl<'c> Filesystem<'c> {
    async fn read_file(&mut self, file: &Path) -> Result<Arc<String>, Arc<anyhow::Error>> {
        #[async_trait]
        impl Key for File {
            type Value = Result<Arc<String>, Arc<anyhow::Error>>;
            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                _cancellations: &CancellationContext,
            ) -> Self::Value {
                let encoding = ctx.encodings().get().await?;

                let s = fs::read_to_string(&self.0).unwrap();

                Ok(Arc::new(match encoding {
                    Encoding::Utf8 => s,
                    Encoding::Ascii => s.replace(":-)", "smile"),
                }))
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            }
        }

        self.0
            .compute(&File(file.to_path_buf()))
            .await
            .map_err(|e| Arc::new(anyhow::anyhow!(e)))?
    }
}

trait HasFilesystem<'c> {
    fn filesystem(&'c mut self) -> Filesystem<'c>;
}

impl<'c> HasFilesystem<'c> for DiceComputations {
    fn filesystem(&'c mut self) -> Filesystem<'c> {
        Filesystem(self)
    }
}

trait SetFilesystem {
    fn filesystem_changed(&mut self, file: &Path) -> anyhow::Result<()>;
}

impl SetFilesystem for DiceTransactionUpdater {
    fn filesystem_changed(&mut self, file: &Path) -> anyhow::Result<()> {
        Ok(self.changed(vec![File(file.to_path_buf())])?)
    }
}

#[tokio::test]
async fn demo() -> anyhow::Result<()> {
    let temp = NamedTempFile::new().unwrap();
    let f = PathBuf::from(temp.path());

    let dice = Dice::builder().build(DetectCycles::Enabled);

    let mut ctx = dice.updater();
    ctx.set_encodings(Encoding::Utf8)?;
    ctx.commit().await;

    let set = |x: &str| fs::write(&f, x).unwrap();

    async fn get(x: &str, dice: &Arc<Dice>, f: &Path) {
        let contents = dice
            .updater()
            .commit()
            .await
            .filesystem()
            .read_file(f)
            .await
            .unwrap();
        assert_eq!(*contents, x)
    }

    set(":-)");

    get(":-)", &dice, &f).await;

    // doesn't change because I didn't dirty it
    set("hello :-)");

    get(":-)", &dice, &f).await;

    let mut ctx = dice.updater();
    ctx.filesystem_changed(&f)?;
    ctx.commit().await;
    get("hello :-)", &dice, &f).await;

    let mut ctx = dice.updater();
    ctx.set_encodings(Encoding::Ascii)?;
    ctx.commit().await;
    get("hello smile", &dice, &f).await;

    Ok(())
}
