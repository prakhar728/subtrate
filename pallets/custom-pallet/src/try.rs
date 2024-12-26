#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{
    pallet_prelude::*,
    traits::{Currency, ExistenceRequirement, WithdrawReasons},
    transactional,
};
use frame_system::pallet_prelude::*;
use sp_runtime::traits::{AccountIdConversion, Saturating, Zero};

pub use pallet::*;

type BalanceOf<T> =
    <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

#[frame_support::pallet]
pub mod pallet {
    use super::*;

    #[pallet::config]
    pub trait Config: frame_system::Config {
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
        type Currency: Currency<Self::AccountId>;
        
        #[pallet::constant]
        type VoteCost: Get<BalanceOf<Self>>;
        
        #[pallet::constant]
        type CreatorRewardPercentage: Get<u32>;
        
        #[pallet::constant]
        type MarketDuration: Get<Self::BlockNumber>;
    }

    #[pallet::pallet]
    pub struct Pallet<T>(_);

    #[pallet::storage]
    pub type Markets<T: Config> = StorageMap<
        _,
        Blake2_128Concat,
        u32,
        Market<T>,
    >;

    #[pallet::storage]
    pub type MarketCount<T: Config> = StorageValue<_, u32, ValueQuery>;

    #[pallet::storage]
    pub type Votes<T: Config> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        u32,  // market_id
        Blake2_128Concat,
        T::AccountId,  // voter
        bool,  // vote (true = yes, false = no)
        ValueQuery,
    >;

    #[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, MaxEncodedLen, TypeInfo)]
    pub struct Market<T: Config> {
        pub creator: T::AccountId,
        pub end_block: T::BlockNumber,
        pub yes_votes: u32,
        pub no_votes: u32,
        pub total_staked: BalanceOf<T>,
        pub is_active: bool,
        pub metadata: BoundedVec<u8, ConstU32<256>>,
    }

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        MarketCreated {
            market_id: u32,
            creator: T::AccountId,
            end_block: T::BlockNumber,
            metadata: BoundedVec<u8, ConstU32<256>>,
        },
        VoteCast {
            market_id: u32,
            voter: T::AccountId,
            vote: bool,
        },
        RewardsDistributed {
            market_id: u32,
            creator: T::AccountId,
            creator_reward: BalanceOf<T>,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        MarketDoesNotExist,
        MarketNotActive,
        MarketStillActive,
        AlreadyVoted,
        InvalidVoteCost,
        MetadataTooLong,
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        #[pallet::weight(10_000)]
        pub fn create_market(
            origin: OriginFor<T>,
            metadata: Vec<u8>,
        ) -> DispatchResult {
            let creator = ensure_signed(origin)?;
            
            let market_id = MarketCount::<T>::get();
            let end_block = frame_system::Pallet::<T>::block_number()
                .saturating_add(T::MarketDuration::get());

            let bounded_metadata: BoundedVec<_, _> = metadata
                .try_into()
                .map_err(|_| Error::<T>::MetadataTooLong)?;

            let market = Market {
                creator: creator.clone(),
                end_block,
                yes_votes: 0,
                no_votes: 0,
                total_staked: Zero::zero(),
                is_active: true,
                metadata: bounded_metadata.clone(),
            };

            Markets::<T>::insert(market_id, market);
            MarketCount::<T>::put(market_id.saturating_add(1));

            Self::deposit_event(Event::MarketCreated {
                market_id,
                creator,
                end_block,
                metadata: bounded_metadata,
            });

            Ok(())
        }

        #[pallet::weight(10_000)]
        #[transactional]
        pub fn vote(
            origin: OriginFor<T>,
            market_id: u32,
            vote_yes: bool,
        ) -> DispatchResult {
            let voter = ensure_signed(origin)?;
            
            let mut market = Markets::<T>::get(market_id)
                .ok_or(Error::<T>::MarketDoesNotExist)?;
            ensure!(market.is_active, Error::<T>::MarketNotActive);
            ensure!(
                frame_system::Pallet::<T>::block_number() <= market.end_block,
                Error::<T>::MarketNotActive
            );
            ensure!(
                !Votes::<T>::contains_key(market_id, &voter),
                Error::<T>::AlreadyVoted
            );

            // Transfer vote cost
            T::Currency::transfer(
                &voter,
                &Self::account_id(),
                T::VoteCost::get(),
                ExistenceRequirement::KeepAlive,
            )?;

            // Record vote
            if vote_yes {
                market.yes_votes = market.yes_votes.saturating_add(1);
            } else {
                market.no_votes = market.no_votes.saturating_add(1);
            }
            market.total_staked = market.total_staked.saturating_add(T::VoteCost::get());

            Markets::<T>::insert(market_id, market);
            Votes::<T>::insert(market_id, &voter, vote_yes);

            Self::deposit_event(Event::VoteCast {
                market_id,
                voter,
                vote: vote_yes,
            });

            Ok(())
        }

        #[pallet::weight(10_000)]
        #[transactional]
        pub fn release_rewards(
            origin: OriginFor<T>,
            market_id: u32,
        ) -> DispatchResult {
            let _ = ensure_signed(origin)?;
            
            let mut market = Markets::<T>::get(market_id)
                .ok_or(Error::<T>::MarketDoesNotExist)?;
            ensure!(market.is_active, Error::<T>::MarketNotActive);
            ensure!(
                frame_system::Pallet::<T>::block_number() > market.end_block,
                Error::<T>::MarketStillActive
            );

            market.is_active = false;
            
            let total_reward_pool = market.total_staked;
            let creator_reward = total_reward_pool
                .saturating_mul(T::CreatorRewardPercentage::get().into())
                .checked_div(100u32.into())
                .unwrap_or_else(Zero::zero);
            
            let remaining_reward_pool = total_reward_pool.saturating_sub(creator_reward);
            let yes_wins = market.yes_votes > market.no_votes;
            let winner_count = if yes_wins { market.yes_votes } else { market.no_votes };
            
            if winner_count > 0 {
                let reward_per_winner = remaining_reward_pool
                    .checked_div(winner_count.into())
                    .unwrap_or_else(Zero::zero);

                // Distribute rewards to winners
                for (voter, vote) in Votes::<T>::iter_prefix(market_id) {
                    if vote == yes_wins {
                        let _ = T::Currency::transfer(
                            &Self::account_id(),
                            &voter,
                            reward_per_winner,
                            ExistenceRequirement::AllowDeath,
                        );
                    }
                }
            }

            // Transfer creator reward
            let _ = T::Currency::transfer(
                &Self::account_id(),
                &market.creator,
                creator_reward,
                ExistenceRequirement::AllowDeath,
            );

            Markets::<T>::insert(market_id, market.clone());

            Self::deposit_event(Event::RewardsDistributed {
                market_id,
                creator: market.creator,
                creator_reward,
            });

            Ok(())
        }
    }

    impl<T: Config> Pallet<T> {
        pub fn account_id() -> T::AccountId {
            T::PalletId::get().into_account_truncating()
        }
    }
}